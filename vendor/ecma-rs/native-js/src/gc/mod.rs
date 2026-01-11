pub mod polling;
pub mod roots;
pub mod statepoint;
pub mod statepoints;

/// Canonical patchpoint/statepoint ID used by `native-js` for all GC statepoints.
///
/// LLVM StackMap v3 records start with a `patchpoint_id: u64`. For
/// `llvm.experimental.gc.statepoint`, that value comes from the first `i64`
/// argument ("id") of the intrinsic.
///
/// On LLVM 18, `rewrite-statepoints-for-gc` uses the fixed default `0xABCDEF00`
/// when no `"statepoint-id"` callsite directive is provided. The native runtime
/// uses this ID as a cheap discriminator in debug-only stackmap verification
/// (`runtime-native::statepoint_verify`) to find the records that follow the
/// statepoint stackmap layout.
///
/// `native-js` keeps this value constant for *all* manually emitted statepoints
/// as well. The callsite return address is the real key for stackmap lookup; the
/// ID is purely a convention/marker.
pub const LLVM_STATEPOINT_PATCHPOINT_ID: u64 = 0xABCDEF00;
