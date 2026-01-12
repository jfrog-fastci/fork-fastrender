use std::{fmt, mem::size_of};

use super::format::{records_semantically_equal, Callsite, LiveOut, Location, StackMapRecord};
use super::statepoint::StatepointRecordView;

const STACKMAP_VERSION: u8 = 3;
const STACKMAP_V3_HEADER_SIZE: usize = 16;

/// Parse-time limits for `.llvm_stackmaps` sections.
///
/// Stackmaps are trusted metadata in our own build pipeline, but `StackMaps::parse` is also used as
/// a general-purpose parser (e.g. in fuzz targets). Treat the input as hostile and cap how much
/// memory we are willing to reserve for parsed structures so corrupted sections can't trigger OOM.
#[derive(Debug, Clone, Copy)]
pub struct ParseOptions {
    /// Maximum input length accepted by the parser.
    pub max_section_bytes: usize,
    /// Maximum amount of memory we will reserve for decoded structures (across the entire parse).
    pub max_alloc_bytes: usize,
    /// Maximum number of function table entries in a single v3 blob.
    pub max_functions_per_blob: usize,
    /// Maximum number of constants in a single v3 blob.
    pub max_constants_per_blob: usize,
    /// Maximum number of records in a single v3 blob.
    pub max_records_per_blob: usize,
    /// Maximum number of locations in a single record.
    pub max_locations_per_record: usize,
    /// Maximum number of live-outs in a single record.
    pub max_live_outs_per_record: usize,
}

impl ParseOptions {
    // 64 MiB is comfortably above the size of stackmaps we expect in practice, while still
    // preventing pathological allocations on arbitrary input bytes.
    pub const DEFAULT_MAX_ALLOC_BYTES: usize = 64 * 1024 * 1024;
    // Also cap the input slice length so we don't spend unbounded time scanning long runs of
    // padding.
    pub const DEFAULT_MAX_SECTION_BYTES: usize = 64 * 1024 * 1024;

    pub const DEFAULT: Self = Self {
        max_section_bytes: Self::DEFAULT_MAX_SECTION_BYTES,
        max_alloc_bytes: Self::DEFAULT_MAX_ALLOC_BYTES,
        // These are primarily sanity limits; the allocation budget is the primary guardrail.
        max_functions_per_blob: 1_000_000,
        max_constants_per_blob: 1_000_000,
        max_records_per_blob: 1_000_000,
        // Record-local counts are u16 in the binary format; keep defaults permissive and allow
        // callers (e.g. fuzz targets) to dial them down.
        max_locations_per_record: u16::MAX as usize,
        max_live_outs_per_record: u16::MAX as usize,
    };

    /// Tighter limits suitable for fuzzing.
    pub const FUZZING: Self = Self {
        max_section_bytes: 1 * 1024 * 1024, // 1 MiB
        max_alloc_bytes: 1 * 1024 * 1024,   // 1 MiB
        max_functions_per_blob: 100_000,
        max_constants_per_blob: 100_000,
        max_records_per_blob: 100_000,
        max_locations_per_record: 4096,
        max_live_outs_per_record: 4096,
    };
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Copy)]
struct ParseBudget {
    remaining: usize,
    cap: usize,
}

impl ParseBudget {
    fn new(cap: usize) -> Self {
        Self {
            remaining: cap,
            cap,
        }
    }

    fn charge_bytes(&mut self, bytes: usize, offset: usize, what: &'static str) -> Result<(), ParseError> {
        if bytes > self.remaining {
            return Err(ParseError::new(
                offset,
                format!(
                    "{what} allocation would exceed parser budget: requested {bytes} bytes, remaining {}, cap {}",
                    self.remaining,
                    self.cap
                ),
            ));
        }
        self.remaining -= bytes;
        Ok(())
    }

    fn charge_elems<T>(&mut self, count: usize, offset: usize, what: &'static str) -> Result<(), ParseError> {
        let bytes = count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| ParseError::new(offset, "allocation size overflow"))?;
        self.charge_bytes(bytes, offset, what)
    }
}

fn reserve_additional<T>(
    vec: &mut Vec<T>,
    additional: usize,
    offset: usize,
    what: &'static str,
) -> Result<(), ParseError> {
    vec.try_reserve_exact(additional).map_err(|e| {
        ParseError::new(
            offset,
            format!("failed to reserve space for {what} ({additional} items): {e}"),
        )
    })
}

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
        let add = align - 1;
        let new_pos = self
            .pos
            .checked_add(add)
            .ok_or_else(|| ParseError::new(self.pos, "offset overflow while aligning"))?
            & !add;
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
/// - + compact (one small `Callsite` entry per record)
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
        Self::parse_with_options(bytes, &ParseOptions::DEFAULT)
    }

    pub fn parse_with_options(bytes: &[u8], options: &ParseOptions) -> Result<Self, ParseError> {
        if bytes.len() > options.max_section_bytes {
            return Err(ParseError::new(
                0,
                format!(
                    ".llvm_stackmaps section too large to parse safely: {} bytes (cap {})",
                    bytes.len(),
                    options.max_section_bytes
                ),
            ));
        }

        let mut budget = ParseBudget::new(options.max_alloc_bytes);
        let mut functions: Vec<StackMapFunction> = Vec::new();
        let mut constants: Vec<u64> = Vec::new();
        let mut records: Vec<StackMapRecord> = Vec::new();
        let mut callsites: Vec<Callsite> = Vec::new();

        // `.llvm_stackmaps` in the final linked binary may contain one or more
        // StackMap v3 blobs concatenated by the linker (one per object file),
        // with alignment padding between blobs. The padding is usually 0-filled,
        // but some toolchains have been observed to insert a few non-zero bytes.
        //
        // See `native-js/docs/stackmaps.md` in this repository for details.
        let mut off = 0usize;
        let mut seen_any_blob = false;
        while off < bytes.len() {
            // Skip alignment/trailing padding.
            while off < bytes.len() && bytes[off] == 0 {
                off += 1;
            }
            if off >= bytes.len() {
                break;
            }
            if bytes.len() - off < STACKMAP_V3_HEADER_SIZE {
                // Too small to contain a full v3 header. Some toolchains can leave short, non-zero
                // alignment noise at the end of the section; ignore it.
                break;
            }

            if bytes[off] != STACKMAP_VERSION {
                // Some toolchains have been observed to insert a few non-zero padding bytes between
                // concatenated v3 blobs. Try to recover by scanning forward for the next plausible
                // header (version=3, reserved bytes = 0).
                //
                // Limit the scan so we don't accidentally resync into the middle of a valid blob
                // if our offset accounting is wrong.
                const MAX_PADDING_SCAN: usize = 256;
                let scan_end = off
                    .saturating_add(MAX_PADDING_SCAN)
                    .min(bytes.len().saturating_sub(STACKMAP_V3_HEADER_SIZE));
                let mut found: Option<usize> = None;
                for i in off + 1..=scan_end {
                    if bytes[i] == STACKMAP_VERSION
                        && bytes[i + 1] == 0
                        && bytes[i + 2] == 0
                        && bytes[i + 3] == 0
                    {
                        found = Some(i);
                        break;
                    }
                }
                if let Some(i) = found {
                    off = i;
                    continue;
                }
            }

            let const_base = u32::try_from(constants.len())
                .map_err(|_| ParseError::new(off, "constants table too large"))?;
            let record_base = records.len();

            let blob = parse_blob(
                &bytes[off..],
                const_base,
                record_base,
                options,
                &mut budget,
                &mut functions,
                &mut constants,
                &mut records,
                &mut callsites,
            )
            .map_err(|mut e| {
                // Adjust blob-local errors to section offsets for clearer diagnostics.
                e.offset = e.offset.saturating_add(off);
                e
            })?;
            let (blob_header, blob_len) = blob;
            let _ = blob_header;

            if blob_len == 0 {
                return Err(ParseError::new(off, "parsed stackmap blob length is 0"));
            }

            off = off
                .checked_add(blob_len)
                .ok_or_else(|| ParseError::new(off, "offset overflow while advancing to next blob"))?;
            seen_any_blob = true;
        }

        if !seen_any_blob {
            return Err(ParseError::new(0, "empty .llvm_stackmaps section"));
        }

        let header = StackMapHeader {
            version: 3,
            num_functions: u32::try_from(functions.len())
                .map_err(|_| ParseError::new(0, "num_functions exceeds u32"))?,
            num_constants: u32::try_from(constants.len())
                .map_err(|_| ParseError::new(0, "num_constants exceeds u32"))?,
            num_records: u32::try_from(records.len())
                .map_err(|_| ParseError::new(0, "num_records exceeds u32"))?,
        };

        // The runtime lookup key is the callsite PC (return address). We generally require this to
        // be unique to avoid ambiguous GC root enumeration.
        //
        // However, lld's identical code folding (`--icf=all`) can fold identical functions into the
        // same machine code address. When those functions each contain a statepoint/patchpoint,
        // LLVM may emit multiple stackmap records whose `function_address + instruction_offset`
        // resolves to the same PC.
        //
        // This is safe iff the records are identical (they describe the same machine instruction).
        // In that case, deduplicate so `lookup(pc)` stays unambiguous.
        callsites.sort_by_key(|e| e.pc);
        if callsites.len() > 1 {
            let mut write: usize = 0;
            let mut read: usize = 0;
            let len = callsites.len();
            while read < len {
                let base = callsites[read];
                let base_rec = records
                    .get(base.record_index)
                    .ok_or_else(|| ParseError::new(0, "callsite record_index out of bounds"))?;

                let mut next = read + 1;
                while next < len && callsites[next].pc == base.pc {
                    let other = callsites[next];
                    if base.function_address != other.function_address || base.stack_size != other.stack_size {
                        return Err(ParseError::new(0, format!("duplicate callsite pc 0x{:x}", base.pc)));
                    }
                    let other_rec = records
                        .get(other.record_index)
                        .ok_or_else(|| ParseError::new(0, "callsite record_index out of bounds"))?;

                    let same_record = records_semantically_equal(base_rec, other_rec);
                    if !same_record {
                        return Err(ParseError::new(0, format!("duplicate callsite pc 0x{:x}", base.pc)));
                    }
                    next += 1;
                }

                callsites[write] = base;
                write += 1;
                read = next;
            }

            callsites.truncate(write);
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

    pub fn lookup_callsite(&self, pc: u64) -> Option<&Callsite> {
        let idx = self.callsites.binary_search_by_key(&pc, |e| e.pc).ok()?;
        self.callsites.get(idx)
    }

    pub fn lookup(&self, pc: u64) -> Option<&StackMapRecord> {
        let rec_idx = self.lookup_callsite(pc)?.record_index;
        self.records.get(rec_idx)
    }

    pub fn lookup_statepoint(&self, pc: u64) -> Option<StatepointRecordView<'_>> {
        let rec = self.lookup(pc)?;
        StatepointRecordView::decode(rec)
    }
}

fn parse_blob(
    bytes: &[u8],
    const_base: u32,
    record_base: usize,
    options: &ParseOptions,
    budget: &mut ParseBudget,
    functions: &mut Vec<StackMapFunction>,
    constants: &mut Vec<u64>,
    records: &mut Vec<StackMapRecord>,
    callsites: &mut Vec<Callsite>,
) -> Result<(StackMapHeader, usize), ParseError> {
    let mut c = Cursor::new(bytes);

    let version = c.read_u8()?;
    if version != STACKMAP_VERSION {
        return Err(ParseError::new(
            0,
            format!("unsupported stackmap version {version} (expected {STACKMAP_VERSION})"),
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

    // These are indices into the *combined* tables across all concatenated blobs.
    let functions_base = functions.len();
    let constants_base = constants.len();
    debug_assert_eq!(records.len(), record_base);
    debug_assert_eq!(u32::try_from(constants_base).ok(), Some(const_base));

    let num_functions_usize =
        usize::try_from(num_functions).map_err(|_| ParseError::new(c.pos(), "num_functions does not fit in usize"))?;
    let num_constants_usize =
        usize::try_from(num_constants).map_err(|_| ParseError::new(c.pos(), "num_constants does not fit in usize"))?;
    let num_records_usize =
        usize::try_from(num_records).map_err(|_| ParseError::new(c.pos(), "num_records does not fit in usize"))?;

    if num_functions_usize > options.max_functions_per_blob {
        return Err(ParseError::new(
            c.pos(),
            format!(
                "num_functions ({num_functions_usize}) exceeds limit ({})",
                options.max_functions_per_blob
            ),
        ));
    }
    if num_constants_usize > options.max_constants_per_blob {
        return Err(ParseError::new(
            c.pos(),
            format!(
                "num_constants ({num_constants_usize}) exceeds limit ({})",
                options.max_constants_per_blob
            ),
        ));
    }
    if num_records_usize > options.max_records_per_blob {
        return Err(ParseError::new(
            c.pos(),
            format!(
                "num_records ({num_records_usize}) exceeds limit ({})",
                options.max_records_per_blob
            ),
        ));
    }

    // Defensive validation of header counts against remaining bytes.
    //
    // Stackmaps are typically trusted metadata (emitted by LLVM in our own build), but these
    // checks prevent pathological allocations if the section is corrupted or if the caller
    // accidentally points at the wrong byte range.
    const FUNCTION_ENTRY_SIZE: usize = 24;
    if num_functions_usize > c.remaining() / FUNCTION_ENTRY_SIZE {
        return Err(ParseError::new(
            c.pos(),
            format!(
                "num_functions ({num_functions_usize}) exceeds remaining bytes ({})",
                c.remaining()
            ),
        ));
    }

    budget.charge_elems::<StackMapFunction>(num_functions_usize, c.pos(), "functions table")?;
    reserve_additional(functions, num_functions_usize, c.pos(), "functions table")?;
    for _ in 0..num_functions_usize {
        functions.push(StackMapFunction {
            address: c.read_u64()?,
            stack_size: c.read_u64()?,
            record_count: c.read_u64()?,
        });
    }

    // The header's num_records should match the sum of per-function record_count.
    let mut expected_records: u64 = 0;
    let functions_end = functions_base
        .checked_add(num_functions_usize)
        .ok_or_else(|| ParseError::new(c.pos(), "functions index overflow"))?;
    for f in &functions[functions_base..functions_end] {
        expected_records = expected_records
            .checked_add(f.record_count)
            .ok_or_else(|| ParseError::new(c.pos(), "record_count overflow while summing functions"))?;
    }
    if expected_records != u64::from(num_records) {
        return Err(ParseError::new(
            c.pos(),
            format!(
                "record count mismatch: functions expect {expected_records}, header says {num_records}"
            ),
        ));
    }

    const CONSTANT_ENTRY_SIZE: usize = 8;
    if num_constants_usize > c.remaining() / CONSTANT_ENTRY_SIZE {
        return Err(ParseError::new(
            c.pos(),
            format!(
                "num_constants ({num_constants_usize}) exceeds remaining bytes ({})",
                c.remaining()
            ),
        ));
    }

    budget.charge_elems::<u64>(num_constants_usize, c.pos(), "constants table")?;
    reserve_additional(constants, num_constants_usize, c.pos(), "constants table")?;
    for _ in 0..num_constants_usize {
        constants.push(c.read_u64()?);
    }

    // Each record is at least 24 bytes, even with 0 locations and 0 live-outs.
    const MIN_RECORD_SIZE: usize = 24;
    if num_records_usize > c.remaining() / MIN_RECORD_SIZE {
        return Err(ParseError::new(
            c.pos(),
            format!(
                "num_records ({num_records_usize}) exceeds remaining bytes ({})",
                c.remaining()
            ),
        ));
    }

    budget.charge_elems::<StackMapRecord>(num_records_usize, c.pos(), "stackmap records")?;
    budget.charge_elems::<Callsite>(num_records_usize, c.pos(), "callsite index")?;
    reserve_additional(records, num_records_usize, c.pos(), "stackmap records")?;
    reserve_additional(callsites, num_records_usize, c.pos(), "callsite index")?;

    let mut seen_records = 0usize;
    for func in &functions[functions_base..functions_end] {
        let record_count = usize::try_from(func.record_count)
            .map_err(|_| ParseError::new(c.pos(), "record_count does not fit in usize"))?;

        for _ in 0..record_count {
            let record_start = c.pos();

            let id = c.read_u64()?;
            let instruction_offset = c.read_u32()?;
            let _reserved = c.read_u16()?;
            let num_locations = c.read_u16()? as usize;

            if num_locations > options.max_locations_per_record {
                return Err(ParseError::new(
                    record_start,
                    format!(
                        "num_locations ({num_locations}) exceeds limit ({})",
                        options.max_locations_per_record
                    ),
                ));
            }

            const LOCATION_ENTRY_SIZE: usize = 12;
            if num_locations > c.remaining() / LOCATION_ENTRY_SIZE {
                return Err(ParseError::new(
                    record_start,
                    format!(
                        "num_locations ({num_locations}) exceeds remaining bytes ({})",
                        c.remaining()
                    ),
                ));
            }

            budget.charge_elems::<Location>(num_locations, record_start, "record locations")?;
            let mut locations: Vec<Location> = Vec::new();
            reserve_additional(&mut locations, num_locations, record_start, "record locations")?;
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
                        let idx_local = u32::try_from(offset_or_val).map_err(|_| {
                            ParseError::new(c.pos(), format!("ConstantIndex is negative: {offset_or_val}"))
                        })?;
                        let value_index = constants_base
                            .checked_add(idx_local as usize)
                            .ok_or_else(|| ParseError::new(c.pos(), "ConstantIndex offset overflow"))?;
                        let value = *constants.get(value_index).ok_or_else(|| {
                            ParseError::new(
                                c.pos(),
                                format!(
                                    "ConstantIndex {idx_local} out of bounds (constants len={})",
                                    num_constants_usize
                                ),
                            )
                        })?;
                        let idx_global = const_base.checked_add(idx_local).ok_or_else(|| {
                            ParseError::new(c.pos(), "ConstantIndex global index overflow")
                        })?;
                        Location::ConstantIndex {
                            size,
                            index: idx_global,
                            value,
                        }
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

            // Live-out header is aligned to an 8-byte boundary after the locations array.
            c.align_to(8)?;
            let _padding = c.read_u16()?;
            let num_live_outs = c.read_u16()? as usize;

            if num_live_outs > options.max_live_outs_per_record {
                return Err(ParseError::new(
                    record_start,
                    format!(
                        "num_live_outs ({num_live_outs}) exceeds limit ({})",
                        options.max_live_outs_per_record
                    ),
                ));
            }

            const LIVEOUT_ENTRY_SIZE: usize = 4;
            if num_live_outs > c.remaining() / LIVEOUT_ENTRY_SIZE {
                return Err(ParseError::new(
                    record_start,
                    format!(
                        "num_live_outs ({num_live_outs}) exceeds remaining bytes ({})",
                        c.remaining()
                    ),
                ));
            }

            budget.charge_elems::<LiveOut>(num_live_outs, record_start, "record live outs")?;
            let mut live_outs: Vec<LiveOut> = Vec::new();
            reserve_additional(&mut live_outs, num_live_outs, record_start, "record live outs")?;
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

            let record_index = record_base
                .checked_add(seen_records)
                .ok_or_else(|| ParseError::new(record_start, "record index overflow"))?;
            records.push(StackMapRecord {
                id,
                instruction_offset,
                callsite_pc,
                locations,
                live_outs,
            });
            callsites.push(Callsite {
                pc: callsite_pc,
                record_index,
                function_address: func.address,
                stack_size: func.stack_size,
            });

            seen_records = seen_records
                .checked_add(1)
                .ok_or_else(|| ParseError::new(record_start, "record counter overflow"))?;
        }
    }

    if seen_records != num_records_usize {
        return Err(ParseError::new(
            c.pos(),
            format!("record count mismatch: header says {num_records_usize}, parsed {seen_records}"),
        ));
    }

    Ok((header, c.pos()))
}

#[cfg(test)]
mod tests {
    use super::StackMaps;
    use crate::stackmap::Location;

    #[test]
    fn rejects_overlarge_num_functions_without_allocating() {
        // Header claims 1000 functions but provides no function table.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&(1000u32).to_le_bytes()); // num functions
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // num records

        let err = StackMaps::parse(&bytes).unwrap_err();
        assert_eq!(err.offset, 16);
    }

    #[test]
    fn rejects_header_record_count_mismatch() {
        // Header says 1 record but function entry says it has 2 records.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

        // Function entry: record_count=2.
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
        bytes.extend_from_slice(&(2u64).to_le_bytes()); // record count

        let err = StackMaps::parse(&bytes).unwrap_err();
        assert_eq!(err.offset, 40);
        assert!(
            err.message.contains("record count mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_overlarge_num_locations_without_allocating() {
        // Record claims 10 locations but the buffer ends immediately after the record header.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

        // Function entry: 1 record.
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
        bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

        // Record header with num_locations=10.
        bytes.extend_from_slice(&(1u64).to_le_bytes()); // id
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // instruction offset
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
        bytes.extend_from_slice(&(10u16).to_le_bytes()); // num locations

        // Provide a minimal record tail (24-byte minimum record size) so the parser reaches the
        // `num_locations` validation rather than failing the blob-level min-size check.
        bytes.extend_from_slice(&[0u8; 8]);

        let err = StackMaps::parse(&bytes).unwrap_err();
        assert_eq!(err.offset, 40);
        assert!(
            err.message.contains("num_locations"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_overlarge_num_live_outs_without_allocating() {
        // Record claims 2 live-outs but provides no live-out entries.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

        // Function entry: 1 record.
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
        bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

        // Record header with num_locations=0.
        bytes.extend_from_slice(&(1u64).to_le_bytes()); // id
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // instruction offset
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

        // Live-out header (already aligned): u16 padding + u16 num_liveouts=2.
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // padding
        bytes.extend_from_slice(&(2u16).to_le_bytes()); // num liveouts

        // Provide a minimal record tail (24-byte minimum record size) so the parser reaches the
        // `num_live_outs` validation rather than failing the blob-level min-size check.
        bytes.extend_from_slice(&[0u8; 4]);

        let err = StackMaps::parse(&bytes).unwrap_err();
        assert_eq!(err.offset, 40);
        assert!(
            err.message.contains("num_live_outs"),
            "unexpected error: {err}"
        );
    }

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

        // StackMap v3 aligns the live-out header to 8 bytes after the locations array.
        // Record size so far: 16 + 12 = 28; pad to 32.
        bytes.extend_from_slice(&[0u8; 4]);

        // Live-out header: u16 padding, u16 num_liveouts.
        bytes.extend_from_slice(&(0xBEEF_u16).to_le_bytes()); // padding (ignored)
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // num liveouts

        // Record end is 8-byte aligned; current record size after header is 36, so pad to 40.
        bytes.extend_from_slice(&[0u8; 4]);

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

    #[test]
    fn parses_live_outs_and_record_padding() {
        // Build a blob with 2 records. Record #0 requires padding before the live-out header (due to
        // an odd number of 12-byte locations) and also padding after the live-out array (even count
        // of 4-byte live-out entries). If we mis-handle alignment, record #1 will not parse.
        let mut bytes = Vec::new();

        // Header
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
        bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
        bytes.extend_from_slice(&(2u32).to_le_bytes()); // num records

        // Function entry
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
        bytes.extend_from_slice(&(2u64).to_le_bytes()); // record count

        // Record #0: 1 location, 2 live-outs.
        bytes.extend_from_slice(&(1u64).to_le_bytes()); // id
        bytes.extend_from_slice(&(10u32).to_le_bytes()); // instruction offset
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
        bytes.extend_from_slice(&(1u16).to_le_bytes()); // num locations

        // Location: Register
        bytes.push(1); // kind
        bytes.push(0); // reserved0
        bytes.extend_from_slice(&(8u16).to_le_bytes()); // size
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reg
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved1
        bytes.extend_from_slice(&(0i32).to_le_bytes()); // offset/val

        // Pad to 8 before live-out header: record header (16) + loc (12) = 28 => +4.
        bytes.extend_from_slice(&[0xAB; 4]);

        // Live-out header: padding + num_liveouts.
        bytes.extend_from_slice(&(0x1234u16).to_le_bytes());
        bytes.extend_from_slice(&(2u16).to_le_bytes()); // two live-outs

        // LiveOut #0
        bytes.extend_from_slice(&(7u16).to_le_bytes());
        bytes.push(0);
        bytes.push(8);
        // LiveOut #1
        bytes.extend_from_slice(&(6u16).to_le_bytes());
        bytes.push(0);
        bytes.push(8);

        // Pad record end to 8: liveout header+entries = 12, record total = 28+4+12=44 => +4.
        bytes.extend_from_slice(&[0xCD; 4]);

        // Record #1: 0 locations, 0 live-outs.
        bytes.extend_from_slice(&(2u64).to_le_bytes()); // id
        bytes.extend_from_slice(&(20u32).to_le_bytes()); // instruction offset
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

        // Already aligned; liveout header:
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // padding
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // num liveouts
        // Pad record end to 8: record header 16 + liveout header 4 = 20 => +4.
        bytes.extend_from_slice(&[0u8; 4]);

        let maps = StackMaps::parse(&bytes).unwrap();
        assert_eq!(maps.records.len(), 2);
        assert_eq!(maps.callsites.len(), 2);

        assert_eq!(maps.lookup(10).unwrap().id, 1);
        assert_eq!(maps.lookup(20).unwrap().id, 2);

        assert_eq!(maps.records[0].live_outs.len(), 2);
        assert_eq!(maps.records[0].live_outs[0].dwarf_reg, 7);
        assert_eq!(maps.records[0].live_outs[0].size, 8);
    }

    #[test]
    fn parses_concatenated_stackmap_blobs() {
        fn build_blob(func_addr: u64, rec_id: u64, inst_off: u32) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&[3, 0, 0, 0]);
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
            bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

            bytes.extend_from_slice(&(func_addr).to_le_bytes());
            bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
            bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

            bytes.extend_from_slice(&(rec_id).to_le_bytes());
            bytes.extend_from_slice(&(inst_off).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

            // Live-out header (already aligned): padding + num_liveouts.
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            // Align record end: 16 + 4 = 20 => +4.
            bytes.extend_from_slice(&[0u8; 4]);

            bytes
        }

        let blob_a = build_blob(0x1000, 1, 0x10);
        let blob_b = build_blob(0x2000, 2, 0x20);

        let mut section = Vec::new();
        section.extend_from_slice(&blob_a);
        section.extend_from_slice(&[0u8; 16]); // padding between blobs
        section.extend_from_slice(&blob_b);
        section.extend_from_slice(&[0u8; 8]); // trailing padding

        let maps = StackMaps::parse(&section).unwrap();
        assert_eq!(maps.records.len(), 2);
        assert_eq!(maps.callsites.len(), 2);
        assert_eq!(maps.lookup(0x1010).unwrap().id, 1);
        assert_eq!(maps.lookup(0x2020).unwrap().id, 2);
    }

    #[test]
    fn deduplicates_identical_duplicate_callsite_pcs() {
        fn build_blob(func_addr: u64, rec_id: u64, inst_off: u32) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&[3, 0, 0, 0]);
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
            bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

            bytes.extend_from_slice(&(func_addr).to_le_bytes());
            bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
            bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

            bytes.extend_from_slice(&(rec_id).to_le_bytes());
            bytes.extend_from_slice(&(inst_off).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

            // Live-out header (already aligned): padding + num_liveouts.
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            // Align record end: 16 + 4 = 20 => +4.
            bytes.extend_from_slice(&[0u8; 4]);

            bytes
        }

        let blob_a = build_blob(0x1000, 1, 0x10);
        let blob_b = build_blob(0x1000, 1, 0x10);

        let mut section = Vec::new();
        section.extend_from_slice(&blob_a);
        section.extend_from_slice(&blob_b);

        let maps = StackMaps::parse(&section).unwrap();
        // Both blobs still contribute records, but the callsite index is deduplicated.
        assert_eq!(maps.records.len(), 2);
        assert_eq!(maps.callsites.len(), 1);
        assert_eq!(maps.lookup(0x1010).unwrap().id, 1);
    }

    #[test]
    fn deduplicates_duplicate_callsite_pcs_even_when_constindex_indices_differ() {
        // When stackmaps are parsed from multiple concatenated blobs, `ConstantIndex` locations are
        // rewritten to refer to the combined constants table. This naturally changes the
        // `ConstantIndex.index` even when the record is otherwise identical.
        //
        // If lld's identical code folding causes callsite PCs to collide, we still want to
        // deduplicate these records (the index is not semantically relevant once the value is
        // resolved).
        fn build_blob(func_addr: u64, rec_id: u64, inst_off: u32, constant: u64) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&[3, 0, 0, 0]);
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num constants
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

            // Function entry: 1 record.
            bytes.extend_from_slice(&(func_addr).to_le_bytes());
            bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
            bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

            // Constants table.
            bytes.extend_from_slice(&(constant).to_le_bytes());

            // Record header with num_locations=1.
            bytes.extend_from_slice(&(rec_id).to_le_bytes());
            bytes.extend_from_slice(&(inst_off).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(1u16).to_le_bytes()); // num locations

            // Location: ConstantIndex(0)
            bytes.push(5); // kind
            bytes.push(0); // reserved0
            bytes.extend_from_slice(&(8u16).to_le_bytes()); // size
            bytes.extend_from_slice(&(0u16).to_le_bytes()); // reg
            bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved1
            bytes.extend_from_slice(&(0i32).to_le_bytes()); // constants[0]

            // Align to 8 before live-out header: record header (16) + loc (12) = 28 => +4.
            bytes.extend_from_slice(&[0u8; 4]);

            // Live-out header: padding + num_liveouts=0.
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());

            // Align record end: 32 + 4 = 36 => +4.
            bytes.extend_from_slice(&[0u8; 4]);

            bytes
        }

        let blob_a = build_blob(0x1000, 1, 0x10, 0x1122_3344_5566_7788);
        let blob_b = build_blob(0x1000, 1, 0x10, 0x1122_3344_5566_7788);

        let mut section = Vec::new();
        section.extend_from_slice(&blob_a);
        section.extend_from_slice(&blob_b);

        let maps = StackMaps::parse(&section).unwrap();
        assert_eq!(maps.records.len(), 2);
        assert_eq!(maps.callsites.len(), 1);

        match (&maps.records[0].locations[0], &maps.records[1].locations[0]) {
            (
                Location::ConstantIndex {
                    index: idx0,
                    value: v0,
                    ..
                },
                Location::ConstantIndex {
                    index: idx1,
                    value: v1,
                    ..
                },
            ) => {
                assert_eq!(*v0, 0x1122_3344_5566_7788);
                assert_eq!(*v1, 0x1122_3344_5566_7788);
                assert_ne!(idx0, idx1, "expected const indices to differ across blobs");
            }
            other => panic!("unexpected locations: {other:?}"),
        }

        assert_eq!(maps.lookup(0x1010).unwrap().id, 1);
    }

    #[test]
    fn rejects_conflicting_duplicate_callsite_pcs() {
        fn build_blob(func_addr: u64, rec_id: u64, inst_off: u32) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&[3, 0, 0, 0]);
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
            bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

            bytes.extend_from_slice(&(func_addr).to_le_bytes());
            bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
            bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

            bytes.extend_from_slice(&(rec_id).to_le_bytes());
            bytes.extend_from_slice(&(inst_off).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

            // Live-out header (already aligned): padding + num_liveouts.
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            // Align record end: 16 + 4 = 20 => +4.
            bytes.extend_from_slice(&[0u8; 4]);

            bytes
        }

        let blob_a = build_blob(0x1000, 1, 0x10);
        let blob_b = build_blob(0x1000, 2, 0x10);

        let mut section = Vec::new();
        section.extend_from_slice(&blob_a);
        section.extend_from_slice(&blob_b);

        let err = StackMaps::parse(&section).unwrap_err();
        assert!(
            err.message.contains("duplicate callsite pc 0x1010"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ignores_short_trailing_non_zero_bytes() {
        fn build_blob(func_addr: u64, rec_id: u64, inst_off: u32) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&[3, 0, 0, 0]);
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
            bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

            bytes.extend_from_slice(&(func_addr).to_le_bytes());
            bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
            bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

            bytes.extend_from_slice(&(rec_id).to_le_bytes());
            bytes.extend_from_slice(&(inst_off).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

            // Live-out header (already aligned): padding + num_liveouts.
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            // Align record end: 16 + 4 = 20 => +4.
            bytes.extend_from_slice(&[0u8; 4]);

            bytes
        }

        let blob = build_blob(0x1000, 1, 0x10);
        let mut section = Vec::new();
        section.extend_from_slice(&blob);
        section.extend_from_slice(&[0xAA; 8]); // short tail (<16B header)

        let maps = StackMaps::parse(&section).unwrap();
        assert_eq!(maps.records.len(), 1);
        assert_eq!(maps.lookup(0x1010).unwrap().id, 1);
    }

    #[test]
    fn skips_short_non_zero_padding_between_blobs() {
        fn build_blob(func_addr: u64, rec_id: u64, inst_off: u32) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&[3, 0, 0, 0]);
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
            bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
            bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

            bytes.extend_from_slice(&(func_addr).to_le_bytes());
            bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
            bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

            bytes.extend_from_slice(&(rec_id).to_le_bytes());
            bytes.extend_from_slice(&(inst_off).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

            // Live-out header (already aligned): padding + num_liveouts.
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            bytes.extend_from_slice(&(0u16).to_le_bytes());
            // Align record end: 16 + 4 = 20 => +4.
            bytes.extend_from_slice(&[0u8; 4]);

            bytes
        }

        let blob_a = build_blob(0x1000, 1, 0x10);
        let blob_b = build_blob(0x2000, 2, 0x20);

        let mut section = Vec::new();
        section.extend_from_slice(&blob_a);
        section.extend_from_slice(&[0xAA; 8]); // non-zero padding between blobs
        section.extend_from_slice(&blob_b);

        let maps = StackMaps::parse(&section).unwrap();
        assert_eq!(maps.records.len(), 2);
        assert_eq!(maps.callsites.len(), 2);
        assert_eq!(maps.lookup(0x1010).unwrap().id, 1);
        assert_eq!(maps.lookup(0x2020).unwrap().id, 2);
    }
} 
