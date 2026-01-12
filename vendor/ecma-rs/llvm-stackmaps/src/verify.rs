//! Offline verifier for `.llvm_stackmaps` (StackMap v3) sections.
//!
//! This module is intended for *debug tooling* and CI verification. It parses the
//! stackmaps section via [`crate::StackMaps::parse`] and then validates a small set
//! of invariants that we rely on for the statepoint/GC pipeline.
//!
//! The verifier is conservative:
//! - It must never panic on malformed input.
//! - It returns a structured report with best-effort diagnostics.

use crate::{Location, LocationKind, ParseError, StackMapRecord, StackMaps, StatepointRecordView};

#[derive(Debug, Clone, Copy)]
pub struct VerifyOptions {
    /// Expected pointer width (in bytes) for GC roots.
    ///
    /// StackMap v3 encodes a `size` per location. For statepoints we expect all GC roots to be
    /// pointer-sized.
    pub pointer_width: u16,

    /// Maximum GC root pairs per statepoint before the verifier flags the record as suspicious.
    ///
    /// This is not a hard correctness requirement (LLVM could theoretically emit more), but it
    /// catches obviously-corrupted stackmaps where a header field causes the decoder to interpret
    /// a large tail of locations as GC roots.
    pub max_gc_roots: usize,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            // `llvm-stackmaps` is only used by 64-bit runtimes today.
            pointer_width: 8,
            max_gc_roots: 4096,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VerificationFailure {
    pub kind: &'static str,
    pub message: String,
    /// Byte offset into the stackmaps byte slice (when known).
    pub offset: Option<usize>,
    /// Callsite PC / return address for record-scoped failures (when known).
    pub pc: Option<u64>,
    /// Function base address associated with the record/callsite (when known).
    pub function_address: Option<u64>,
    /// Record index into [`StackMaps::records`] (when known).
    pub record_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct VerificationReport {
    pub functions: usize,
    pub constants: usize,
    pub records: usize,
    pub callsites: usize,
    pub decoded_statepoints: usize,
    pub failures: Vec<VerificationFailure>,
}

impl VerificationReport {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// Deterministic JSON summary intended for CI/log parsing.
    ///
    /// Notes:
    /// - PCs are rendered as hex strings to avoid JSON number precision loss.
    /// - Field order is stable.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push('{');

        out.push_str("\"ok\":");
        out.push_str(if self.ok() { "true" } else { "false" });

        out.push_str(",\"counts\":{");
        out.push_str("\"functions\":");
        out.push_str(&self.functions.to_string());
        out.push_str(",\"constants\":");
        out.push_str(&self.constants.to_string());
        out.push_str(",\"records\":");
        out.push_str(&self.records.to_string());
        out.push_str(",\"callsites\":");
        out.push_str(&self.callsites.to_string());
        out.push('}');

        out.push_str(",\"decoded_statepoints\":");
        out.push_str(&self.decoded_statepoints.to_string());

        out.push_str(",\"failures\":[");
        for (i, f) in self.failures.iter().enumerate() {
            if i != 0 {
                out.push(',');
            }
            out.push('{');

            out.push_str("\"kind\":");
            write_json_string(&mut out, f.kind);

            out.push_str(",\"message\":");
            write_json_string(&mut out, &f.message);

            out.push_str(",\"offset\":");
            match f.offset {
                Some(off) => out.push_str(&off.to_string()),
                None => out.push_str("null"),
            }

            out.push_str(",\"pc\":");
            match f.pc {
                Some(pc) => write_json_string(&mut out, &format!("0x{pc:x}")),
                None => out.push_str("null"),
            }

            out.push_str(",\"function\":");
            match f.function_address {
                Some(addr) => write_json_string(&mut out, &format!("0x{addr:x}")),
                None => out.push_str("null"),
            }

            out.push_str(",\"record_index\":");
            match f.record_index {
                Some(idx) => out.push_str(&idx.to_string()),
                None => out.push_str("null"),
            }

            out.push('}');
        }
        out.push_str("]}");

        out
    }
}

pub fn verify_stackmaps_bytes(bytes: &[u8], opts: VerifyOptions) -> VerificationReport {
    let mut report = VerificationReport {
        functions: 0,
        constants: 0,
        records: 0,
        callsites: 0,
        decoded_statepoints: 0,
        failures: Vec::new(),
    };

    let maps = match StackMaps::parse(bytes) {
        Ok(m) => m,
        Err(e) => {
            report.failures.push(VerificationFailure {
                kind: "parse_error",
                message: e.message,
                offset: Some(e.offset),
                pc: None,
                function_address: None,
                record_index: None,
            });
            return report;
        }
    };

    report.functions = maps.functions.len();
    report.constants = maps.constants.len();
    report.records = maps.records.len();
    report.callsites = maps.callsites().len();

    let record_function_info = map_record_function_info(&maps);

    // Best-effort mapping from record index to section byte offsets for actionable diagnostics.
    let record_offsets: Option<Vec<usize>> = match scan_record_offsets(bytes) {
        Ok(v) => {
            if v.len() != maps.records.len() {
                report.failures.push(VerificationFailure {
                    kind: "offset_map_mismatch",
                    message: format!(
                        "record offset scan produced {} records, but parser returned {} records",
                        v.len(),
                        maps.records.len()
                    ),
                    offset: None,
                    pc: None,
                    function_address: None,
                    record_index: None,
                });
                None
            } else {
                Some(v)
            }
        }
        Err(e) => {
            report.failures.push(VerificationFailure {
                kind: "offset_scan_error",
                message: e.message,
                offset: Some(e.offset),
                pc: None,
                function_address: None,
                record_index: None,
            });
            None
        }
    };

    // Callsite index invariants.
    verify_callsites_sorted_and_unique(&maps, &mut report, &record_offsets);
    verify_callsite_record_linkage(&maps, &mut report, &record_offsets, &record_function_info);

    // Statepoint-specific invariants.
    verify_statepoints(
        &maps,
        opts,
        &mut report,
        &record_offsets,
        &record_function_info,
    );

    report
}

fn verify_callsites_sorted_and_unique(
    maps: &StackMaps,
    report: &mut VerificationReport,
    record_offsets: &Option<Vec<usize>>,
) {
    let callsites = maps.callsites();
    for w in callsites.windows(2) {
        let a = w[0].pc;
        let b = w[1].pc;
        if a > b {
            report.failures.push(VerificationFailure {
                kind: "callsites_unsorted",
                message: format!("callsites are not sorted: 0x{a:x} > 0x{b:x}"),
                offset: None,
                pc: Some(a),
                function_address: None,
                record_index: None,
            });
            break;
        }
        if a == b {
            report.failures.push(VerificationFailure {
                kind: "duplicate_callsite_pc",
                message: format!("duplicate callsite pc in index: 0x{a:x}"),
                offset: None,
                pc: Some(a),
                function_address: None,
                record_index: None,
            });
            break;
        }
    }

    // Sanity: if callsites exist, binary search should succeed for each.
    for c in callsites {
        if maps.lookup_callsite(c.pc).is_none() {
            report.failures.push(VerificationFailure {
                kind: "callsite_lookup_failed",
                message: format!("binary search lookup failed for callsite pc 0x{:x}", c.pc),
                offset: None,
                pc: Some(c.pc),
                function_address: Some(c.function_address),
                record_index: Some(c.record_index),
            });
            break;
        }
    }

    // Avoid unused warning when record_offsets isn't used in this function (for future expansion).
    let _ = record_offsets;
}

fn verify_callsite_record_linkage(
    maps: &StackMaps,
    report: &mut VerificationReport,
    record_offsets: &Option<Vec<usize>>,
    record_function_info: &[Option<RecordFunctionInfo>],
) {
    for callsite in maps.callsites() {
        let Some(rec) = maps.records.get(callsite.record_index) else {
            report.failures.push(VerificationFailure {
                kind: "callsite_record_oob",
                message: format!(
                    "callsite record_index out of bounds: {} (records.len()={})",
                    callsite.record_index,
                    maps.records.len()
                ),
                offset: None,
                pc: Some(callsite.pc),
                function_address: Some(callsite.function_address),
                record_index: Some(callsite.record_index),
            });
            continue;
        };

        let expected_func = record_function_info
            .get(callsite.record_index)
            .and_then(|v| *v);
        if let Some(info) = expected_func {
            if info.address != callsite.function_address {
                report.failures.push(VerificationFailure {
                    kind: "callsite_function_address_mismatch",
                    message: format!(
                        "callsite function_address mismatch: expected 0x{:x}, got 0x{:x}",
                        info.address, callsite.function_address
                    ),
                    offset: record_offsets
                        .as_ref()
                        .and_then(|v| v.get(callsite.record_index).copied()),
                    pc: Some(callsite.pc),
                    function_address: Some(callsite.function_address),
                    record_index: Some(callsite.record_index),
                });
            }
            if info.stack_size != callsite.stack_size {
                report.failures.push(VerificationFailure {
                    kind: "callsite_stack_size_mismatch",
                    message: format!(
                        "callsite stack_size mismatch: expected {}, got {}",
                        info.stack_size, callsite.stack_size
                    ),
                    offset: record_offsets
                        .as_ref()
                        .and_then(|v| v.get(callsite.record_index).copied()),
                    pc: Some(callsite.pc),
                    function_address: Some(callsite.function_address),
                    record_index: Some(callsite.record_index),
                });
            }
        } else {
            report.failures.push(VerificationFailure {
                kind: "callsite_record_function_info_missing",
                message: "failed to map record_index back to a function entry".to_string(),
                offset: record_offsets
                    .as_ref()
                    .and_then(|v| v.get(callsite.record_index).copied()),
                pc: Some(callsite.pc),
                function_address: Some(callsite.function_address),
                record_index: Some(callsite.record_index),
            });
        }

        // Recompute pc and check for overflow.
        let expected_pc = match callsite
            .function_address
            .checked_add(u64::from(rec.instruction_offset))
        {
            Some(v) => v,
            None => {
                report.failures.push(VerificationFailure {
                    kind: "callsite_pc_overflow",
                    message: format!(
                        "callsite pc overflow: func=0x{:x} + instruction_offset={}",
                        callsite.function_address, rec.instruction_offset
                    ),
                    offset: record_offsets
                        .as_ref()
                        .and_then(|v| v.get(callsite.record_index).copied()),
                    pc: Some(callsite.pc),
                    function_address: Some(callsite.function_address),
                    record_index: Some(callsite.record_index),
                });
                continue;
            }
        };

        if expected_pc != callsite.pc {
            report.failures.push(VerificationFailure {
                kind: "callsite_pc_mismatch",
                message: format!(
                    "callsite pc mismatch: expected 0x{expected_pc:x} (func=0x{:x}+off={}), got 0x{:x}",
                    callsite.function_address, rec.instruction_offset, callsite.pc
                ),
                offset: record_offsets
                    .as_ref()
                    .and_then(|v| v.get(callsite.record_index).copied()),
                pc: Some(callsite.pc),
                function_address: Some(callsite.function_address),
                record_index: Some(callsite.record_index),
            });
        }

        if rec.callsite_pc != callsite.pc {
            report.failures.push(VerificationFailure {
                kind: "record_callsite_pc_mismatch",
                message: format!(
                    "record.callsite_pc mismatch: record has 0x{:x}, callsite index has 0x{:x}",
                    rec.callsite_pc, callsite.pc
                ),
                offset: record_offsets
                    .as_ref()
                    .and_then(|v| v.get(callsite.record_index).copied()),
                pc: Some(callsite.pc),
                function_address: Some(callsite.function_address),
                record_index: Some(callsite.record_index),
            });
        }
    }
}

fn verify_statepoints(
    maps: &StackMaps,
    opts: VerifyOptions,
    report: &mut VerificationReport,
    record_offsets: &Option<Vec<usize>>,
    record_function_info: &[Option<RecordFunctionInfo>],
) {
    for (record_index, rec) in maps.records.iter().enumerate() {
        if !looks_like_statepoint_record(rec) {
            continue;
        }

        let Some(sp) = StatepointRecordView::decode(rec) else {
            report.failures.push(VerificationFailure {
                kind: "statepoint_decode_failed",
                message: "record has constant statepoint header but does not match expected statepoint layout".to_string(),
                offset: record_offsets.as_ref().and_then(|v| v.get(record_index).copied()),
                pc: Some(rec.callsite_pc),
                function_address: record_function_info
                    .get(record_index)
                    .and_then(|v| v.map(|i| i.address)),
                record_index: Some(record_index),
            });
            continue;
        };

        report.decoded_statepoints = report.decoded_statepoints.saturating_add(1);

        // Header sanity. LLVM defines `flags` as a small bitmask (currently 2 bits).
        if sp.flags > 3 {
            report.failures.push(VerificationFailure {
                kind: "statepoint_flags_out_of_range",
                message: format!("statepoint flags out of range: {} (expected 0..=3)", sp.flags),
                offset: record_offsets.as_ref().and_then(|v| v.get(record_index).copied()),
                pc: Some(rec.callsite_pc),
                function_address: record_function_info
                    .get(record_index)
                    .and_then(|v| v.map(|i| i.address)),
                record_index: Some(record_index),
            });
        }

        let gc_root_count = sp.num_gc_roots();
        if gc_root_count > opts.max_gc_roots {
            report.failures.push(VerificationFailure {
                kind: "statepoint_gc_root_count_unreasonable",
                message: format!(
                    "unreasonable gc root count: {gc_root_count} (max {})",
                    opts.max_gc_roots
                ),
                offset: record_offsets.as_ref().and_then(|v| v.get(record_index).copied()),
                pc: Some(rec.callsite_pc),
                function_address: record_function_info
                    .get(record_index)
                    .and_then(|v| v.map(|i| i.address)),
                record_index: Some(record_index),
            });
        }

        for (pair_idx, pair) in sp.gc_root_pairs().enumerate() {
            let function_address = record_function_info
                .get(record_index)
                .and_then(|v| v.map(|i| i.address));
            verify_gc_root_location(
                opts.pointer_width,
                report,
                record_offsets.as_ref().and_then(|v| v.get(record_index).copied()),
                rec.callsite_pc,
                function_address,
                record_index,
                pair_idx,
                "base",
                pair.base,
            );
            verify_gc_root_location(
                opts.pointer_width,
                report,
                record_offsets.as_ref().and_then(|v| v.get(record_index).copied()),
                rec.callsite_pc,
                function_address,
                record_index,
                pair_idx,
                "derived",
                pair.derived,
            );
        }
    }
}

fn verify_gc_root_location(
    pointer_width: u16,
    report: &mut VerificationReport,
    record_offset: Option<usize>,
    pc: u64,
    function_address: Option<u64>,
    record_index: usize,
    pair_index: usize,
    role: &'static str,
    loc: &Location,
) {
    let kind = loc.kind();
    // For GC roots, the runtime expects an addressable location: either a register root or a stack
    // slot / memory reference. Constants should never appear as relocation targets.
    let supported_root_kind = matches!(
        kind,
        LocationKind::Register | LocationKind::Direct | LocationKind::Indirect
    );
    if !supported_root_kind {
        report.failures.push(VerificationFailure {
            kind: "gc_root_unsupported_location_kind",
            message: format!(
                "gc root {role}#{pair_index} uses unsupported location kind {kind:?} ({loc})"
            ),
            offset: record_offset,
            pc: Some(pc),
            function_address,
            record_index: Some(record_index),
        });
        return;
    }

    let size = loc.size();
    if size != pointer_width {
        report.failures.push(VerificationFailure {
            kind: "gc_root_size_mismatch",
            message: format!(
                "gc root size mismatch for {role}#{pair_index}: expected {pointer_width}, got {size} (kind={kind:?})"
            ),
            offset: record_offset,
            pc: Some(pc),
            function_address,
            record_index: Some(record_index),
        });
    }
}

fn looks_like_statepoint_record(rec: &StackMapRecord) -> bool {
    // Heuristic: LLVM statepoint records start with three constant header locations:
    // (callconv, flags, num_deopt_args).
    let locs = rec.locations();
    if locs.len() < 3 {
        return false;
    }
    locs[0].as_u64().is_some() && locs[1].as_u64().is_some() && locs[2].as_u64().is_some()
}

fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{20}' => {
                // Control characters must be escaped in JSON strings.
                let v = c as u32;
                out.push_str("\\u");
                out.push_str(&format!("{v:04x}"));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// -------------------------------------------------------------------------------------------------
// Record offset scanning
// -------------------------------------------------------------------------------------------------

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
        let end = start.checked_add(n).ok_or_else(|| ParseError {
            offset: start,
            message: "offset overflow".to_string(),
        })?;
        let slice = self.bytes.get(start..end).ok_or_else(|| ParseError {
            offset: start,
            message: "unexpected EOF".to_string(),
        })?;
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

    fn read_u64(&mut self) -> Result<u64, ParseError> {
        let b = self.read_exact(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn align_to(&mut self, align: usize) -> Result<(), ParseError> {
        if !align.is_power_of_two() {
            return Err(ParseError {
                offset: self.pos,
                message: "align must be power of two".to_string(),
            });
        }
        let new_pos = (self.pos + (align - 1)) & !(align - 1);
        if new_pos > self.bytes.len() {
            return Err(ParseError {
                offset: self.pos,
                message: "unexpected EOF while aligning".to_string(),
            });
        }
        self.pos = new_pos;
        Ok(())
    }
}

/// Scan the stackmaps section and return the start offset (within `bytes`) of every record header.
///
/// This intentionally mirrors the blob concatenation/padding behavior of [`StackMaps::parse`], but
/// only extracts offsets (it does not build full record structures).
fn scan_record_offsets(bytes: &[u8]) -> Result<Vec<usize>, ParseError> {
    const STACKMAP_VERSION: u8 = 3;
    const STACKMAP_V3_HEADER_SIZE: usize = 16;
    const FUNCTION_ENTRY_SIZE: usize = 24;
    const CONSTANT_ENTRY_SIZE: usize = 8;
    const LOCATION_ENTRY_SIZE: usize = 12;
    const LIVEOUT_ENTRY_SIZE: usize = 4;

    let mut out: Vec<usize> = Vec::new();
    let mut off: usize = 0;
    let mut seen_any_blob = false;

    while off < bytes.len() {
        while off < bytes.len() && bytes[off] == 0 {
            off += 1;
        }
        if off >= bytes.len() {
            break;
        }
        if bytes.len() - off < STACKMAP_V3_HEADER_SIZE {
            // Too small to contain a full header; treat as trailing noise.
            break;
        }

        if bytes[off] != STACKMAP_VERSION {
            const MAX_PADDING_SCAN: usize = 256;
            let scan_end =
                (off + MAX_PADDING_SCAN).min(bytes.len().saturating_sub(STACKMAP_V3_HEADER_SIZE));
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

        // Parse one blob starting at `off`.
        let mut c = Cursor::new(&bytes[off..]);

        let version = c.read_u8()?;
        if version != STACKMAP_VERSION {
            return Err(ParseError {
                offset: off,
                message: format!("unsupported stackmap version {version}"),
            });
        }
        let _reserved0 = c.read_u8()?;
        let _reserved1 = c.read_u16()?;

        let num_functions = c.read_u32()?;
        let num_constants = c.read_u32()?;
        let num_records = c.read_u32()?;

        let num_functions_usize = usize::try_from(num_functions).map_err(|_| ParseError {
            offset: off + c.pos(),
            message: "num_functions does not fit in usize".to_string(),
        })?;
        let num_constants_usize = usize::try_from(num_constants).map_err(|_| ParseError {
            offset: off + c.pos(),
            message: "num_constants does not fit in usize".to_string(),
        })?;
        let num_records_usize = usize::try_from(num_records).map_err(|_| ParseError {
            offset: off + c.pos(),
            message: "num_records does not fit in usize".to_string(),
        })?;

        // Function table.
        if num_functions_usize > c.remaining() / FUNCTION_ENTRY_SIZE {
            return Err(ParseError {
                offset: off + c.pos(),
                message: "num_functions exceeds remaining bytes".to_string(),
            });
        }
        let mut functions: Vec<(u64, u64)> = Vec::with_capacity(num_functions_usize);
        for _ in 0..num_functions_usize {
            let address = c.read_u64()?;
            let _stack_size = c.read_u64()?;
            let record_count = c.read_u64()?;
            functions.push((address, record_count));
        }

        // Constants.
        if num_constants_usize > c.remaining() / CONSTANT_ENTRY_SIZE {
            return Err(ParseError {
                offset: off + c.pos(),
                message: "num_constants exceeds remaining bytes".to_string(),
            });
        }
        c.read_exact(num_constants_usize * CONSTANT_ENTRY_SIZE)?;

        // Records (in function order, keyed by record_count).
        let mut seen_records: usize = 0;
        for (func_addr, record_count_u64) in &functions {
            let record_count =
                usize::try_from(*record_count_u64).map_err(|_| ParseError {
                    offset: off + c.pos(),
                    message: "record_count does not fit in usize".to_string(),
                })?;
            for _ in 0..record_count {
                let record_start = c.pos();
                // id
                c.read_u64()?;
                let instruction_offset = c.read_u32()?;
                let _reserved = c.read_u16()?;
                let num_locations = c.read_u16()? as usize;

                if num_locations > c.remaining() / LOCATION_ENTRY_SIZE {
                    return Err(ParseError {
                        offset: off + record_start,
                        message: "num_locations exceeds remaining bytes".to_string(),
                    });
                }
                c.read_exact(num_locations * LOCATION_ENTRY_SIZE)?;

                c.align_to(8)?;
                let _padding = c.read_u16()?;
                let num_liveouts = c.read_u16()? as usize;
                if num_liveouts > c.remaining() / LIVEOUT_ENTRY_SIZE {
                    return Err(ParseError {
                        offset: off + record_start,
                        message: "num_live_outs exceeds remaining bytes".to_string(),
                    });
                }
                c.read_exact(num_liveouts * LIVEOUT_ENTRY_SIZE)?;
                c.align_to(8)?;

                // Validate callsite_pc computation doesn't overflow (mirrors parser).
                let _ = (*func_addr)
                    .checked_add(u64::from(instruction_offset))
                    .ok_or_else(|| ParseError {
                        offset: off + record_start,
                        message: "callsite_pc overflow".to_string(),
                    })?;

                out.push(off.checked_add(record_start).ok_or_else(|| ParseError {
                    offset: off,
                    message: "record_start offset overflow".to_string(),
                })?);

                seen_records = seen_records.checked_add(1).ok_or_else(|| ParseError {
                    offset: off + record_start,
                    message: "record counter overflow".to_string(),
                })?;
            }
        }

        if seen_records != num_records_usize {
            return Err(ParseError {
                offset: off + c.pos(),
                message: format!(
                    "record count mismatch: header says {num_records_usize}, scanned {seen_records}"
                ),
            });
        }

        let blob_len = c.pos();
        if blob_len == 0 {
            return Err(ParseError {
                offset: off,
                message: "parsed stackmap blob length is 0".to_string(),
            });
        }

        off = off.checked_add(blob_len).ok_or_else(|| ParseError {
            offset: off,
            message: "offset overflow while advancing to next blob".to_string(),
        })?;
        seen_any_blob = true;
    }

    if !seen_any_blob {
        return Err(ParseError {
            offset: 0,
            message: "empty .llvm_stackmaps section".to_string(),
        });
    }

    Ok(out)
}

#[derive(Debug, Clone, Copy)]
struct RecordFunctionInfo {
    address: u64,
    stack_size: u64,
}

fn map_record_function_info(maps: &StackMaps) -> Vec<Option<RecordFunctionInfo>> {
    let mut out: Vec<Option<RecordFunctionInfo>> = vec![None; maps.records.len()];

    let mut record_index: usize = 0;
    for func in &maps.functions {
        let record_count = match usize::try_from(func.record_count) {
            Ok(v) => v,
            Err(_) => break,
        };
        for _ in 0..record_count {
            let Some(slot) = out.get_mut(record_index) else {
                return out;
            };
            *slot = Some(RecordFunctionInfo {
                address: func.address,
                stack_size: func.stack_size,
            });
            record_index += 1;
        }
    }

    out
}
