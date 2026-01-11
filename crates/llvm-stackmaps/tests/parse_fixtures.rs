use llvm_stackmaps::StackMapsSection;

// These fixtures are raw `.llvm_stackmaps` section bytes extracted from a *linked* ELF
// produced by LLVM 18.
//
// They intentionally cover:
// - multiple callsite records per function (RecordCount > 1)
// - multiple functions per stackmap table (NumFunctions > 1)
// - duplicate Record IDs (PatchPoint IDs) across distinct callsites
const TWO_STATEPOINTS: &[u8] =
    include_bytes!("fixtures/llvm18_stackmaps/two_statepoints.stackmaps.bin");
const TWO_FUNCS: &[u8] = include_bytes!("fixtures/llvm18_stackmaps/two_funcs.stackmaps.bin");

#[test]
fn parses_two_statepoints_one_function() {
    let section = StackMapsSection::parse(TWO_STATEPOINTS).expect("parse stackmaps");
    assert_eq!(section.tables.len(), 1);
    assert_eq!(section.callsites_by_pc.len(), 2);

    let table = &section.tables[0];
    assert_eq!(table.functions.len(), 1);
    assert_eq!(table.functions[0].record_count, 2);
    assert_ne!(
        table.functions[0].address, 0,
        "fixture should come from linked ELF (addresses resolved)"
    );

    let callsites = &section.callsites[table.callsite_range.clone()];
    assert_eq!(callsites.len(), 2);

    // Both callsites belong to the same function.
    assert_eq!(callsites[0].function_index, 0);
    assert_eq!(callsites[1].function_index, 0);
    assert_eq!(callsites[0].function_address, table.functions[0].address);
    assert_eq!(callsites[1].function_address, table.functions[0].address);

    // LLVM 18 can emit multiple records with the same Record ID.
    assert_eq!(callsites[0].record_id, callsites[1].record_id);

    // Distinct callsites in the same function show up as distinct instruction offsets.
    assert_eq!(callsites[0].instruction_offset, 10);
    assert_eq!(callsites[1].instruction_offset, 15);

    let base = table.functions[0].address;
    assert_eq!(callsites[0].callsite_pc, base + 10);
    assert_eq!(callsites[1].callsite_pc, base + 15);
    assert_ne!(callsites[0].callsite_pc, callsites[1].callsite_pc);

    // Lookup uses callsite_pc, not record_id, so both are present.
    assert!(section.callsites_by_pc.contains_key(&callsites[0].callsite_pc));
    assert!(section.callsites_by_pc.contains_key(&callsites[1].callsite_pc));
}

#[test]
fn parses_two_functions_one_record_each() {
    let section = StackMapsSection::parse(TWO_FUNCS).expect("parse stackmaps");
    assert_eq!(section.tables.len(), 1);
    assert_eq!(section.callsites_by_pc.len(), 2);

    let table = &section.tables[0];
    assert_eq!(table.functions.len(), 2);
    assert_eq!(table.functions[0].record_count, 1);
    assert_eq!(table.functions[1].record_count, 1);
    assert_ne!(
        table.functions[0].address, 0,
        "fixture should come from linked ELF (addresses resolved)"
    );
    assert_ne!(
        table.functions[1].address, 0,
        "fixture should come from linked ELF (addresses resolved)"
    );
    assert_ne!(
        table.functions[0].address, table.functions[1].address,
        "functions must have distinct addresses in linked ELF"
    );

    let callsites = &section.callsites[table.callsite_range.clone()];
    assert_eq!(callsites.len(), 2);

    // The stackmap format does not store function identity on each record; association is by
    // each function's RecordCount. We expect the first record to be for the first function,
    // and the second record for the second function.
    assert_eq!(callsites[0].function_index, 0);
    assert_eq!(callsites[1].function_index, 1);

    // Both statepoints use the same Record ID (PatchPoint ID) in practice.
    assert_eq!(callsites[0].record_id, callsites[1].record_id);

    // Both callsites have the same instruction offset within their respective functions.
    assert_eq!(callsites[0].instruction_offset, 10);
    assert_eq!(callsites[1].instruction_offset, 10);

    assert_eq!(callsites[0].function_address, table.functions[0].address);
    assert_eq!(callsites[1].function_address, table.functions[1].address);

    assert_eq!(callsites[0].callsite_pc, table.functions[0].address + 10);
    assert_eq!(callsites[1].callsite_pc, table.functions[1].address + 10);
    assert_ne!(callsites[0].callsite_pc, callsites[1].callsite_pc);

    // Lookup uses callsite_pc, not record_id, so both are present.
    let idx0 = section.callsites_by_pc[&callsites[0].callsite_pc];
    let idx1 = section.callsites_by_pc[&callsites[1].callsite_pc];
    assert_ne!(idx0, idx1);
}
