//! Parser for LLVM's `.llvm_stackmaps` section (StackMap v3).
//!
//! The runtime lookup key for a callsite record is the *return address*:
//! `callsite_pc = function_address + instruction_offset`.
//!
//! Note: LLVM does **not** guarantee that `record_id` (a.k.a. PatchPoint ID) is
//! unique. LLVM 18 can emit the same ID for multiple statepoints, even within
//! the same function. Consumers must therefore index by `callsite_pc`, not by
//! `record_id`.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::ops::Range;

/// Parsed contents of a `.llvm_stackmaps` section.
///
/// The ELF section can contain multiple StackMap tables concatenated together
/// (e.g. one per codegen unit/object file). We parse all tables found.
#[derive(Debug)]
pub struct StackMapsSection {
    pub tables: Vec<StackMapTable>,
    /// Flattened list of all callsite records in table order.
    pub callsites: Vec<CallSiteRecord>,
    /// Map from callsite PC (return address) to the corresponding record.
    ///
    /// This is the lookup key used at runtime when walking stack frames (you
    /// have a return address, not a patchpoint ID).
    pub callsites_by_pc: HashMap<u64, usize>,
}

#[derive(Debug)]
pub struct StackMapTable {
    pub version: u8,
    pub functions: Vec<FunctionRecord>,
    pub constants: Vec<u64>,
    /// Range in [`StackMapsSection::callsites`] containing the callsites for
    /// this table.
    pub callsite_range: Range<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct FunctionRecord {
    pub address: u64,
    pub stack_size: u64,
    pub record_count: u64,
}

#[derive(Debug)]
pub struct CallSiteRecord {
    pub table_index: usize,
    pub function_index: usize,
    pub function_address: u64,
    pub stack_size: u64,
    /// PatchPoint ID / Record ID.
    pub record_id: u64,
    /// Offset from function start to the callsite PC (return address).
    pub instruction_offset: u32,
    /// `function_address + instruction_offset`
    pub callsite_pc: u64,
    pub locations: Vec<Location>,
    pub live_outs: Vec<LiveOut>,
}

#[derive(Debug, Clone, Copy)]
pub struct Location {
    pub kind: u8,
    pub size: u16,
    pub dwarf_reg_num: u16,
    pub offset: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct LiveOut {
    pub dwarf_reg_num: u16,
    pub size: u8,
}

#[derive(Debug)]
pub enum StackMapParseError {
    UnexpectedEof { offset: usize, wanted: usize },
    InvalidVersion { offset: usize, version: u8 },
    RecordCountSumMismatch {
        table_index: usize,
        num_records: usize,
        sum_record_counts: usize,
    },
    RecordCountOverflow { table_index: usize, function_index: usize, record_count: u64 },
    CallsitePcOverflow {
        table_index: usize,
        function_index: usize,
        function_address: u64,
        instruction_offset: u32,
    },
    DuplicateCallsitePc { callsite_pc: u64 },
    TrailingNonZeroBytes { offset: usize },
}

impl fmt::Display for StackMapParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { offset, wanted } => {
                write!(f, "unexpected end of file at offset {offset}, wanted {wanted} bytes")
            }
            Self::InvalidVersion { offset, version } => {
                write!(f, "invalid StackMap version {version} at offset {offset} (expected 3)")
            }
            Self::RecordCountSumMismatch {
                table_index,
                num_records,
                sum_record_counts,
            } => write!(
                f,
                "stackmap table {table_index}: NumRecords={num_records} but sum(RecordCount)={sum_record_counts}"
            ),
            Self::RecordCountOverflow {
                table_index,
                function_index,
                record_count,
            } => write!(
                f,
                "stackmap table {table_index} function {function_index}: RecordCount={record_count} does not fit in usize"
            ),
            Self::CallsitePcOverflow {
                table_index,
                function_index,
                function_address,
                instruction_offset,
            } => write!(
                f,
                "stackmap table {table_index} function {function_index}: callsite_pc overflow: {function_address} + {instruction_offset}"
            ),
            Self::DuplicateCallsitePc { callsite_pc } => {
                write!(f, "duplicate callsite_pc {callsite_pc}")
            }
            Self::TrailingNonZeroBytes { offset } => {
                write!(f, "trailing non-zero bytes starting at offset {offset}")
            }
        }
    }
}

impl Error for StackMapParseError {}

impl StackMapsSection {
    pub fn parse(bytes: &[u8]) -> Result<Self, StackMapParseError> {
        let mut r = Reader::new(bytes);

        let mut tables = Vec::new();
        let mut callsites = Vec::new();
        let mut callsites_by_pc = HashMap::new();

        while r.remaining() > 0 {
            r.skip_while_zero();
            if r.remaining() == 0 {
                break;
            }

            let table_index = tables.len();
            let table_start = r.offset();

            let version = r.read_u8()?;
            if version != 3 {
                return Err(StackMapParseError::InvalidVersion {
                    offset: table_start,
                    version,
                });
            }

            let _reserved0 = r.read_u8()?;
            let _reserved1 = r.read_u16()?;

            let num_functions = r.read_u32()? as usize;
            let num_constants = r.read_u32()? as usize;
            let num_records = r.read_u32()? as usize;

            let mut functions = Vec::with_capacity(num_functions);
            for _ in 0..num_functions {
                functions.push(FunctionRecord {
                    address: r.read_u64()?,
                    stack_size: r.read_u64()?,
                    record_count: r.read_u64()?,
                });
            }

            let mut constants = Vec::with_capacity(num_constants);
            for _ in 0..num_constants {
                constants.push(r.read_u64()?);
            }

            let mut raw_records = Vec::with_capacity(num_records);
            for _ in 0..num_records {
                raw_records.push(parse_record(&mut r)?);
            }

            let callsite_start = callsites.len();

            let sum_record_counts = functions
                .iter()
                .enumerate()
                .try_fold(0usize, |acc, (function_index, func)| {
                    let count: usize = func.record_count.try_into().map_err(|_| {
                        StackMapParseError::RecordCountOverflow {
                            table_index,
                            function_index,
                            record_count: func.record_count,
                        }
                    })?;
                    Ok(acc + count)
                })?;

            if sum_record_counts != num_records {
                return Err(StackMapParseError::RecordCountSumMismatch {
                    table_index,
                    num_records,
                    sum_record_counts,
                });
            }

            // Associate callsite records to their functions using each function's RecordCount.
            // Do NOT use record_id here: LLVM does not guarantee uniqueness.
            let mut raw_iter = raw_records.into_iter();
            for (function_index, func) in functions.iter().enumerate() {
                let count: usize = func.record_count.try_into().map_err(|_| {
                    StackMapParseError::RecordCountOverflow {
                        table_index,
                        function_index,
                        record_count: func.record_count,
                    }
                })?;

                for _ in 0..count {
                    let rec = raw_iter
                        .next()
                        .expect("sum_record_counts already validated");

                    let callsite_pc = func
                        .address
                        .checked_add(rec.instruction_offset as u64)
                        .ok_or(StackMapParseError::CallsitePcOverflow {
                            table_index,
                            function_index,
                            function_address: func.address,
                            instruction_offset: rec.instruction_offset,
                        })?;

                    let idx = callsites.len();
                    callsites.push(CallSiteRecord {
                        table_index,
                        function_index,
                        function_address: func.address,
                        stack_size: func.stack_size,
                        record_id: rec.record_id,
                        instruction_offset: rec.instruction_offset,
                        callsite_pc,
                        locations: rec.locations,
                        live_outs: rec.live_outs,
                    });

                    if callsites_by_pc.insert(callsite_pc, idx).is_some() {
                        return Err(StackMapParseError::DuplicateCallsitePc { callsite_pc });
                    }
                }
            }

            let callsite_end = callsites.len();
            debug_assert_eq!(callsite_end - callsite_start, num_records);

            tables.push(StackMapTable {
                version,
                functions,
                constants,
                callsite_range: callsite_start..callsite_end,
            });
        }

        // The `.llvm_stackmaps` section can be padded out by the linker/assembler;
        // allow trailing zeros but not other bytes.
        if r.remaining() > 0 && !r.remaining_slice().iter().all(|b| *b == 0) {
            return Err(StackMapParseError::TrailingNonZeroBytes { offset: r.offset() });
        }

        Ok(Self {
            tables,
            callsites,
            callsites_by_pc,
        })
    }
}

#[derive(Debug)]
struct RawRecord {
    record_id: u64,
    instruction_offset: u32,
    locations: Vec<Location>,
    live_outs: Vec<LiveOut>,
}

fn parse_record(r: &mut Reader<'_>) -> Result<RawRecord, StackMapParseError> {
    let record_id = r.read_u64()?;
    let instruction_offset = r.read_u32()?;
    let _reserved = r.read_u16()?;
    let num_locations = r.read_u16()? as usize;

    let mut locations = Vec::with_capacity(num_locations);
    for _ in 0..num_locations {
        let kind = r.read_u8()?;
        let _reserved0 = r.read_u8()?;
        let size = r.read_u16()?;
        let dwarf_reg_num = r.read_u16()?;
        let _reserved1 = r.read_u16()?;
        let offset = r.read_i32()?;
        locations.push(Location {
            kind,
            size,
            dwarf_reg_num,
            offset,
        });
    }

    r.align_to(8)?;

    let num_live_outs = r.read_u16()? as usize;
    let _reserved = r.read_u16()?;

    let mut live_outs = Vec::with_capacity(num_live_outs);
    for _ in 0..num_live_outs {
        let dwarf_reg_num = r.read_u16()?;
        let _reserved = r.read_u8()?;
        let size = r.read_u8()?;
        live_outs.push(LiveOut { dwarf_reg_num, size });
    }

    r.align_to(8)?;

    Ok(RawRecord {
        record_id,
        instruction_offset,
        locations,
        live_outs,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn offset(&self) -> usize {
        self.offset
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn remaining_slice(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    fn skip_while_zero(&mut self) {
        while self.offset < self.bytes.len() && self.bytes[self.offset] == 0 {
            self.offset += 1;
        }
    }

    fn align_to(&mut self, align: usize) -> Result<(), StackMapParseError> {
        let rem = self.offset % align;
        if rem == 0 {
            return Ok(());
        }
        let skip = align - rem;
        self.skip(skip)
    }

    fn skip(&mut self, n: usize) -> Result<(), StackMapParseError> {
        if self.remaining() < n {
            return Err(StackMapParseError::UnexpectedEof {
                offset: self.offset,
                wanted: n,
            });
        }
        self.offset += n;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, StackMapParseError> {
        let bytes = self.read_exact::<1>()?;
        Ok(bytes[0])
    }

    fn read_u16(&mut self) -> Result<u16, StackMapParseError> {
        let bytes = self.read_exact::<2>()?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32, StackMapParseError> {
        let bytes = self.read_exact::<4>()?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_i32(&mut self) -> Result<i32, StackMapParseError> {
        let bytes = self.read_exact::<4>()?;
        Ok(i32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64, StackMapParseError> {
        let bytes = self.read_exact::<8>()?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], StackMapParseError> {
        if self.remaining() < N {
            return Err(StackMapParseError::UnexpectedEof {
                offset: self.offset,
                wanted: N,
            });
        }
        let bytes = &self.bytes[self.offset..self.offset + N];
        self.offset += N;

        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }
}
