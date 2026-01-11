use std::fmt;

use super::format::{Callsite, LiveOut, Location, StackMapRecord};
use super::statepoint::StatepointRecordView;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl ParseError {
    fn new(offset: usize, message: impl Into<String>) -> Self {
        Self {
            offset,
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stackmap parse error at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Copy)]
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn read_exact(&mut self, n: usize) -> Result<&'a [u8], ParseError> {
        let start = self.pos;
        let end = start.checked_add(n).ok_or_else(|| ParseError::new(start, "offset overflow"))?;
        let slice = self
            .bytes
            .get(start..end)
            .ok_or_else(|| ParseError::new(start, "unexpected EOF"))?;
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, ParseError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, ParseError> {
        let b = self.read_exact(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, ParseError> {
        let b = self.read_exact(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_i32(&mut self) -> Result<i32, ParseError> {
        let b = self.read_exact(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_u64(&mut self) -> Result<u64, ParseError> {
        let b = self.read_exact(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn align_to(&mut self, align: usize) -> Result<(), ParseError> {
        if !align.is_power_of_two() {
            return Err(ParseError::new(self.pos, "align must be a power of two"));
        }
        let new_pos = (self.pos + (align - 1)) & !(align - 1);
        if new_pos > self.bytes.len() {
            return Err(ParseError::new(self.pos, "unexpected EOF while aligning"));
        }
        self.pos = new_pos;
        Ok(())
    }
}

/// StackMap header (v3).
#[derive(Debug, Clone, Copy)]
pub struct StackMapHeader {
    pub version: u8,
    pub num_functions: u32,
    pub num_constants: u32,
    pub num_records: u32,
}

/// Function entry in the StackMap section.
#[derive(Debug, Clone, Copy)]
pub struct StackMapFunction {
    pub address: u64,
    pub stack_size: u64,
    pub record_count: u64,
}

/// Parsed StackMap section contents.
///
/// Lookup is implemented as a sorted `Vec` of callsites + binary search:
/// - + compact (one `u64` + `usize` per record)
/// - + deterministic iteration order
/// - - `O(log n)` lookup vs. `O(1)` average for a hash map
#[derive(Debug, Clone)]
pub struct StackMaps {
    pub header: StackMapHeader,
    pub functions: Vec<StackMapFunction>,
    pub constants: Vec<u64>,
    pub records: Vec<StackMapRecord>,
    callsites: Vec<Callsite>,
}

impl StackMaps {
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut c = Cursor::new(bytes);

        let version = c.read_u8()?;
        if version != 3 {
            return Err(ParseError::new(
                0,
                format!("unsupported stackmap version {version} (expected 3)"),
            ));
        }
        let _reserved0 = c.read_u8()?;
        let _reserved1 = c.read_u16()?;

        let num_functions = c.read_u32()?;
        let num_constants = c.read_u32()?;
        let num_records = c.read_u32()?;

        let header = StackMapHeader {
            version,
            num_functions,
            num_constants,
            num_records,
        };

        let num_functions_usize = usize::try_from(num_functions).map_err(|_| {
            ParseError::new(c.pos(), "num_functions does not fit in usize")
        })?;
        let num_constants_usize = usize::try_from(num_constants).map_err(|_| {
            ParseError::new(c.pos(), "num_constants does not fit in usize")
        })?;
        let num_records_usize =
            usize::try_from(num_records).map_err(|_| ParseError::new(c.pos(), "num_records does not fit in usize"))?;

        let mut functions = Vec::with_capacity(num_functions_usize);
        for _ in 0..num_functions_usize {
            functions.push(StackMapFunction {
                address: c.read_u64()?,
                stack_size: c.read_u64()?,
                record_count: c.read_u64()?,
            });
        }

        let mut constants = Vec::with_capacity(num_constants_usize);
        for _ in 0..num_constants_usize {
            constants.push(c.read_u64()?);
        }

        let mut records = Vec::with_capacity(num_records_usize);
        let mut callsites = Vec::with_capacity(num_records_usize);

        let mut seen_records = 0usize;
        for func in &functions {
            let record_count = usize::try_from(func.record_count).map_err(|_| {
                ParseError::new(c.pos(), "record_count does not fit in usize")
            })?;

            for _ in 0..record_count {
                let record_start = c.pos();

                let id = c.read_u64()?;
                let instruction_offset = c.read_u32()?;
                let _reserved = c.read_u16()?;
                let num_locations = c.read_u16()? as usize;

                let mut locations = Vec::with_capacity(num_locations);
                for _ in 0..num_locations {
                    let kind = c.read_u8()?;
                    let _reserved0 = c.read_u8()?;
                    let size = c.read_u16()?;
                    let dwarf_reg = c.read_u16()?;
                    let _reserved1 = c.read_u16()?;
                    let offset_or_val = c.read_i32()?;

                    // StackMap v3 LocationKind values (LLVM 18):
                    //   1 = Register
                    //   2 = Direct
                    //   3 = Indirect
                    //   4 = Constant
                    //   5 = ConstantIndex
                    let loc = match kind {
                        1 => Location::Register { size, dwarf_reg },
                        2 => Location::Direct {
                            size,
                            dwarf_reg,
                            offset: offset_or_val,
                        },
                        3 => Location::Indirect {
                            size,
                            dwarf_reg,
                            offset: offset_or_val,
                        },
                        4 => Location::Constant {
                            size,
                            value: i64::from(offset_or_val),
                        },
                        5 => {
                            let idx = u32::try_from(offset_or_val).map_err(|_| {
                                ParseError::new(
                                    c.pos(),
                                    format!("ConstantIndex is negative: {offset_or_val}"),
                                )
                            })?;
                            let value = *constants.get(idx as usize).ok_or_else(|| {
                                ParseError::new(
                                    c.pos(),
                                    format!(
                                        "ConstantIndex {idx} out of bounds (constants len={})",
                                        constants.len()
                                    ),
                                )
                            })?;
                            Location::ConstantIndex { size, index: idx, value }
                        }
                        other => {
                            return Err(ParseError::new(
                                c.pos(),
                                format!("unknown location kind {other}"),
                            ))
                        }
                    };
                    locations.push(loc);
                }

                let num_live_outs = c.read_u16()? as usize;
                let _reserved = c.read_u16()?;
                let mut live_outs = Vec::with_capacity(num_live_outs);
                for _ in 0..num_live_outs {
                    let dwarf_reg = c.read_u16()?;
                    let _reserved = c.read_u8()?;
                    let size = c.read_u8()?;
                    live_outs.push(LiveOut { dwarf_reg, size });
                }

                // Records are padded to 8-byte alignment.
                c.align_to(8)?;

                let callsite_pc = func
                    .address
                    .checked_add(u64::from(instruction_offset))
                    .ok_or_else(|| ParseError::new(record_start, "callsite_pc overflow"))?;

                let record_index = records.len();
                records.push(StackMapRecord {
                    id,
                    instruction_offset,
                    callsite_pc,
                    locations,
                    live_outs,
                });
                callsites.push(Callsite { pc: callsite_pc, record_index });

                seen_records = seen_records
                    .checked_add(1)
                    .ok_or_else(|| ParseError::new(record_start, "record counter overflow"))?;
            }
        }

        if seen_records != num_records_usize {
            return Err(ParseError::new(
                c.pos(),
                format!(
                    "record count mismatch: header says {num_records_usize}, parsed {seen_records}"
                ),
            ));
        }

        // If multiple functions have overlapping address ranges (e.g. in an object file with
        // relocations stripped), the computed callsite PC may collide. That's ambiguous at runtime,
        // so treat duplicates as an error.
        callsites.sort_by_key(|e| e.pc);
        for w in callsites.windows(2) {
            if w[0].pc == w[1].pc {
                return Err(ParseError::new(
                    0,
                    format!("duplicate callsite pc 0x{:x}", w[0].pc),
                ));
            }
        }

        if c.remaining() != 0 {
            // There can be trailing padding in practice; tolerate it if it's all zeros.
            if c.bytes[c.pos()..].iter().any(|&b| b != 0) {
                return Err(ParseError::new(c.pos(), "trailing non-zero bytes"));
            }
        }

        Ok(Self {
            header,
            functions,
            constants,
            records,
            callsites,
        })
    }

    pub fn callsites(&self) -> &[Callsite] {
        &self.callsites
    }

    pub fn lookup(&self, pc: u64) -> Option<&StackMapRecord> {
        let idx = self
            .callsites
            .binary_search_by_key(&pc, |e| e.pc)
            .ok()?;
        let rec_idx = self.callsites[idx].record_index;
        self.records.get(rec_idx)
    }

    pub fn lookup_statepoint(&self, pc: u64) -> Option<StatepointRecordView<'_>> {
        let rec = self.lookup(pc)?;
        StatepointRecordView::decode(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::StackMaps;
    use crate::stackmap::Location;

    #[test]
    fn parse_constant_index_location() {
        // Minimal stackmap section:
        // - 1 function
        // - 1 constant
        // - 1 record with 1 location: ConstantIndex(0)
        let mut bytes = Vec::new();

        // Header
        bytes.extend_from_slice(&[
            3,  // version
            0,  // reserved0
            0, 0, // reserved1
        ]);
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num constants
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

        // Function entry
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
        bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

        // Constants table
        bytes.extend_from_slice(&(0x1122_3344_5566_7788u64).to_le_bytes());

        // Record
        bytes.extend_from_slice(&(7u64).to_le_bytes()); // id
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // instruction offset
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
        bytes.extend_from_slice(&(1u16).to_le_bytes()); // num locations

        // Location (ConstantIndex kind = 5)
        bytes.push(5); // kind
        bytes.push(0); // reserved0
        bytes.extend_from_slice(&(8u16).to_le_bytes()); // size
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reg
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved1
        bytes.extend_from_slice(&(0i32).to_le_bytes()); // index 0

        // Live-outs
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // num liveouts
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved

        // Align record to 8 bytes (record size so far: 16 + 12 + 4 = 32, already aligned)

        let maps = StackMaps::parse(&bytes).unwrap();
        assert_eq!(maps.records.len(), 1);
        assert_eq!(maps.callsites.len(), 1);
        assert_eq!(maps.constants.len(), 1);
        assert_eq!(maps.callsites[0].pc, 0);
        assert_eq!(maps.records[0].id, 7);
        assert_eq!(maps.records[0].locations.len(), 1);
        match &maps.records[0].locations[0] {
            Location::ConstantIndex { index, value, .. } => {
                assert_eq!(*index, 0);
                assert_eq!(*value, 0x1122_3344_5566_7788);
            }
            other => panic!("unexpected location: {other:?}"),
        }
    }
}
