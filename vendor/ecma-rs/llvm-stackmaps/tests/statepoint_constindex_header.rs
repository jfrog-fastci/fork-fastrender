use llvm_stackmaps::{Location, StackMapRecord, StatepointRecordView};

#[test]
fn statepoint_accepts_constantindex_header_constants() {
    // LLVM stackmap records may encode statepoint header constants via the constants table
    // (`ConstantIndex`) rather than inline `Constant` values. Ensure our statepoint decoder
    // treats both forms equivalently.
    let record = StackMapRecord {
        id: 0,
        instruction_offset: 0,
        callsite_pc: 0,
        locations: vec![
            Location::ConstantIndex {
                size: 8,
                index: 0,
                value: 8,
            }, // callconv (fastcc)
            Location::ConstantIndex {
                size: 8,
                index: 1,
                value: 1,
            }, // flags
            Location::ConstantIndex {
                size: 8,
                index: 2,
                value: 2,
            }, // deopt_count
            // deopt args (arbitrary)
            Location::Constant { size: 8, value: 11 },
            Location::Constant { size: 8, value: 22 },
            // one (base, derived) GC pair
            Location::Indirect {
                size: 8,
                dwarf_reg: 7,
                offset: 8,
            },
            Location::Indirect {
                size: 8,
                dwarf_reg: 7,
                offset: 8,
            },
        ],
        live_outs: vec![],
    };

    let sp = StatepointRecordView::decode(&record).expect("decode statepoint");
    assert_eq!(sp.call_conv, 8);
    assert_eq!(sp.flags, 1);
    assert_eq!(sp.deopt_args.len(), 2);
    assert_eq!(sp.deopt_args[0].as_u64(), Some(11));
    assert_eq!(sp.deopt_args[1].as_u64(), Some(22));
    assert_eq!(sp.num_gc_roots(), 1);
}

