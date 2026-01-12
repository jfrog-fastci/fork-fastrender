use std::fmt;

use super::format::{Location, StackMapRecord};

#[derive(Debug, Clone)]
pub enum StatepointDecodeError {
    TooFewLocations { locations_len: usize },
    HeaderNotConstant {
        /// 1-based header index (callconv=1, flags=2, deopt_count=3).
        header_index: usize,
        found_kind: &'static str,
    },
    HeaderConstantNegative {
        /// 1-based header index (callconv=1, flags=2, deopt_count=3).
        header_index: usize,
        value: i64,
    },
    DeoptCountTooLarge { deopt_count: u64 },
    DeoptCountExceedsLocations {
        deopt_count: usize,
        remaining_locations: usize,
    },
    OddGcLocationCount { gc_locations_len: usize },
}

impl fmt::Display for StatepointDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatepointDecodeError::TooFewLocations { locations_len } => {
                write!(
                    f,
                    "statepoint stackmap record must contain at least 3 locations (found {locations_len})"
                )
            }
            StatepointDecodeError::HeaderNotConstant {
                header_index,
                found_kind,
            } => write!(
                f,
                "statepoint header location #{header_index} must be Constant/ConstantIndex (found {found_kind})"
            ),
            StatepointDecodeError::HeaderConstantNegative {
                header_index,
                value,
            } => write!(
                f,
                "statepoint header location #{header_index} is a negative constant ({value})"
            ),
            StatepointDecodeError::DeoptCountTooLarge { deopt_count } => {
                write!(f, "statepoint deopt count {deopt_count} does not fit usize")
            }
            StatepointDecodeError::DeoptCountExceedsLocations {
                deopt_count,
                remaining_locations,
            } => write!(
                f,
                "statepoint declares {deopt_count} deopt locations but only {remaining_locations} locations remain after the 3-entry header"
            ),
            StatepointDecodeError::OddGcLocationCount { gc_locations_len } => write!(
                f,
                "statepoint has {gc_locations_len} trailing locations after header+deopt; expected an even number for (base, derived) pairs"
            ),
        }
    }
}

impl std::error::Error for StatepointDecodeError {}

fn kind_name(loc: &Location) -> &'static str {
    match loc {
        Location::Register { .. } => "Register",
        Location::Direct { .. } => "Direct",
        Location::Indirect { .. } => "Indirect",
        Location::Constant { .. } => "Constant",
        Location::ConstantIndex { .. } => "ConstantIndex",
    }
}

fn header_constant_u64(locations: &[Location], header_index: usize) -> Result<u64, StatepointDecodeError> {
    let loc = locations
        .get(header_index)
        .ok_or(StatepointDecodeError::TooFewLocations {
            locations_len: locations.len(),
        })?;
    match loc {
        Location::Constant { value, .. } => u64::try_from(*value).map_err(|_| {
            StatepointDecodeError::HeaderConstantNegative {
                header_index: header_index + 1,
                value: *value,
            }
        }),
        Location::ConstantIndex { value, .. } => Ok(*value),
        other => Err(StatepointDecodeError::HeaderNotConstant {
            header_index: header_index + 1,
            found_kind: kind_name(other),
        }),
    }
}

/// A `(base, derived)` relocation pair for a single GC pointer.
///
/// LLVM statepoints model derived pointers explicitly (e.g. interior pointers).
/// The runtime typically uses the base pointer to identify the owning object,
/// and updates the derived pointer after relocation.
#[derive(Debug, Clone, Copy)]
pub struct GcRootPair<'a> {
    pub base: &'a Location,
    pub derived: &'a Location,
}

/// A decoded view of a `gc.statepoint` StackMap record.
///
/// Layout (as emitted by LLVM 18 / StackMap v3 for statepoints):
/// - 3 header constants:
///   1. callconv
///   2. flags
///   3. num_deopt_args
/// - `num_deopt_args` locations for deoptimization state (ignored by our GC)
/// - remaining locations are `(base, derived)` relocation pairs
#[derive(Debug, Clone, Copy)]
pub struct StatepointRecordView<'a> {
    pub call_conv: u64,
    pub flags: u64,
    pub deopt_args: &'a [Location],
    gc_roots_flat: &'a [Location],
}

impl<'a> StatepointRecordView<'a> {
    pub fn try_decode(record: &'a StackMapRecord) -> Result<Self, StatepointDecodeError> {
        let locs = record.locations();
        if locs.len() < 3 {
            return Err(StatepointDecodeError::TooFewLocations {
                locations_len: locs.len(),
            });
        }

        let call_conv = header_constant_u64(locs, 0)?;
        let flags = header_constant_u64(locs, 1)?;
        let num_deopt_u64 = header_constant_u64(locs, 2)?;
        let num_deopt =
            usize::try_from(num_deopt_u64).map_err(|_| StatepointDecodeError::DeoptCountTooLarge {
                deopt_count: num_deopt_u64,
            })?;

        let header_len = 3usize;
        let deopt_end = header_len
            .checked_add(num_deopt)
            .ok_or(StatepointDecodeError::DeoptCountTooLarge {
                deopt_count: num_deopt_u64,
            })?;
        let remaining_locations = locs.len().saturating_sub(header_len);
        if num_deopt > remaining_locations {
            return Err(StatepointDecodeError::DeoptCountExceedsLocations {
                deopt_count: num_deopt,
                remaining_locations,
            });
        }

        let deopt_args = &locs[header_len..deopt_end];
        let gc_roots_flat = &locs[deopt_end..];

        if gc_roots_flat.len() % 2 != 0 {
            return Err(StatepointDecodeError::OddGcLocationCount {
                gc_locations_len: gc_roots_flat.len(),
            });
        }

        Ok(Self {
            call_conv,
            flags,
            deopt_args,
            gc_roots_flat,
        })
    }

    pub fn decode(record: &'a StackMapRecord) -> Option<Self> {
        Self::try_decode(record).ok()
    }

    pub fn num_gc_roots(&self) -> usize {
        self.gc_roots_flat.len() / 2
    }

    pub fn gc_root_pairs(&self) -> impl Iterator<Item = GcRootPair<'a>> + 'a {
        self.gc_roots_flat.chunks_exact(2).map(|pair| GcRootPair {
            base: &pair[0],
            derived: &pair[1],
        })
    }
}
