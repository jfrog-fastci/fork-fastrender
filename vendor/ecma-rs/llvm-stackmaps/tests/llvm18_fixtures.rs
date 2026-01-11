use llvm_stackmaps::StackMaps;

// These fixtures are raw `.llvm_stackmaps` section bytes extracted from a *linked* ELF produced
// by LLVM 18. The function addresses are therefore already resolved (non-zero), avoiding any
// relocation handling in parser tests.
//
// They intentionally cover LLVM 18 behaviors observed in the wild:
// - `FunctionRecord.RecordCount > 1` when a function has multiple statepoints
// - multiple records with the *same* Record ID (PatchPoint ID)
// - multiple functions within a single stackmap table (`NumFunctions > 1`), with records
//   associated to functions purely via `RecordCount`
const TWO_STATEPOINTS: &[u8] =
    include_bytes!("fixtures/llvm18_stackmaps/two_statepoints.stackmaps.bin");
const TWO_FUNCS: &[u8] = include_bytes!("fixtures/llvm18_stackmaps/two_funcs.stackmaps.bin");

#[test]
fn llvm18_fixture_two_statepoints_has_two_callsites_even_with_duplicate_record_id() {
    let maps = StackMaps::parse(TWO_STATEPOINTS).expect("parse stackmaps");

    assert_eq!(maps.header.version, 3);
    assert_eq!(maps.header.num_functions, 1);
    assert_eq!(maps.header.num_records, 2);

    assert_eq!(maps.functions.len(), 1);
    let f = maps.functions[0];
    assert_eq!(f.record_count, 2);
    assert_ne!(
        f.address, 0,
        "fixture should come from linked ELF (addresses resolved)"
    );

    assert_eq!(maps.records.len(), 2);
    assert_eq!(maps.callsites().len(), 2);

    // LLVM 18 can emit multiple statepoint records with the same Record ID.
    assert_eq!(maps.records[0].id, maps.records[1].id);

    // Callsite entries carry per-function metadata for stack walking.
    assert_eq!(maps.callsites()[0].function_address, f.address);
    assert_eq!(maps.callsites()[0].stack_size, f.stack_size);
    assert_eq!(maps.callsites()[1].function_address, f.address);
    assert_eq!(maps.callsites()[1].stack_size, f.stack_size);

    assert_eq!(maps.records[0].instruction_offset, 10);
    assert_eq!(maps.records[1].instruction_offset, 15);

    assert_eq!(maps.records[0].callsite_pc, f.address + 10);
    assert_eq!(maps.records[1].callsite_pc, f.address + 15);
    assert_ne!(maps.records[0].callsite_pc, maps.records[1].callsite_pc);

    // Lookup is keyed by callsite PC (return address), not by record ID.
    for rec in &maps.records {
        let looked = maps.lookup(rec.callsite_pc).expect("lookup by callsite_pc");
        assert_eq!(looked.callsite_pc, rec.callsite_pc);
        assert_eq!(looked.id, rec.id);
    }
}

#[test]
fn llvm18_fixture_two_functions_associates_records_via_record_count_not_record_id() {
    let maps = StackMaps::parse(TWO_FUNCS).expect("parse stackmaps");

    assert_eq!(maps.header.version, 3);
    assert_eq!(maps.header.num_functions, 2);
    assert_eq!(maps.header.num_records, 2);

    assert_eq!(maps.functions.len(), 2);
    assert_eq!(maps.records.len(), 2);
    assert_eq!(maps.callsites().len(), 2);

    // Each function has exactly one callsite record.
    assert_eq!(maps.functions[0].record_count, 1);
    assert_eq!(maps.functions[1].record_count, 1);

    assert_ne!(
        maps.functions[0].address, 0,
        "fixture should come from linked ELF (addresses resolved)"
    );
    assert_ne!(
        maps.functions[1].address, 0,
        "fixture should come from linked ELF (addresses resolved)"
    );
    assert_ne!(
        maps.functions[0].address, maps.functions[1].address,
        "functions must have distinct addresses in linked ELF"
    );

    // Record IDs are not guaranteed unique (both are the same here).
    assert_eq!(maps.records[0].id, maps.records[1].id);

    // Both statepoints have the same instruction offset within their respective functions.
    assert_eq!(maps.records[0].instruction_offset, 10);
    assert_eq!(maps.records[1].instruction_offset, 10);

    let pc0 = maps.functions[0].address + 10;
    let pc1 = maps.functions[1].address + 10;
    assert_ne!(pc0, pc1);

    // Correct association uses each function's RecordCount.
    assert_eq!(maps.records[0].callsite_pc, pc0);
    assert_eq!(maps.records[1].callsite_pc, pc1);

    // Both callsites are indexed by PC, despite identical record IDs.
    assert_eq!(maps.lookup(pc0).unwrap().callsite_pc, pc0);
    assert_eq!(maps.lookup(pc1).unwrap().callsite_pc, pc1);

    // And the per-callsite metadata points back at the correct function.
    let mut callsites = maps.callsites().iter().copied().collect::<Vec<_>>();
    callsites.sort_by_key(|c| c.function_address);
    assert_eq!(callsites.len(), 2);
    assert_eq!(callsites[0].function_address, maps.functions[0].address);
    assert_eq!(callsites[0].stack_size, maps.functions[0].stack_size);
    assert_eq!(callsites[1].function_address, maps.functions[1].address);
    assert_eq!(callsites[1].stack_size, maps.functions[1].stack_size);
}
