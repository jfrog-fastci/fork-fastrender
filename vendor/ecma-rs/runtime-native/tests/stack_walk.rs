use runtime_native::gc_roots::StackRootEnumerator;
use runtime_native::stackmaps::StackMaps;

#[test]
fn frame_pointer_stack_walker_and_slot_addressing() {
  // Simulate a small stack region with two frames:
  // [callee_fp] -> saved caller fp
  // [callee_fp+8] -> return address
  // caller_sp at callsite = callee_fp + 16
  let mut stack = vec![0usize; 64];
  let base = stack.as_mut_ptr() as usize;

  let callee_fp = base + 8 * std::mem::size_of::<usize>();
  let caller_fp = base + 24 * std::mem::size_of::<usize>();
  let return_address = 0x1234usize;

  unsafe {
    // Callee frame header.
    (callee_fp as *mut usize).write(caller_fp);
    (callee_fp as *mut usize).add(1).write(return_address);

    // Caller frame header (terminates chain).
    (caller_fp as *mut usize).write(0);
    (caller_fp as *mut usize).add(1).write(0);

    // Simulate two pointer slots in caller frame at offsets 0 and 8 from caller_sp.
    let caller_sp = callee_fp + 16;
    let base_slot_addr = caller_sp as *mut usize;
    let derived_slot_addr = (caller_sp + 8) as *mut usize;
    base_slot_addr.write(0xAAA0);
    derived_slot_addr.write(0xAAA8);

    let stackmaps = StackMaps::parse(&minimal_stackmap_section(return_address as u32)).unwrap();
    let roots = StackRootEnumerator::new(&stackmaps);

    let mut seen = vec![];
    roots.visit_reloc_pairs(callee_fp, |pair| {
      seen.push((pair.base_slot as usize, pair.derived_slot as usize));
    });

    assert_eq!(seen, vec![(base_slot_addr as usize, derived_slot_addr as usize)]);
  }
}

fn minimal_stackmap_section(instruction_offset: u32) -> Vec<u8> {
  // Builds a minimal StackMap v3 section containing one function and one record with:
  // - 3 constant header locations
  // - 1 (base, derived) Indirect pair at [RSP+0] and [RSP+8]
  //
  // This is intentionally tiny so the unit test doesn't depend on external LLVM tools.
  let mut bytes = Vec::new();

  fn push_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
  }
  fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn align_to(out: &mut Vec<u8>, align: usize) {
    while out.len() % align != 0 {
      out.push(0);
    }
  }

  // Header.
  push_u8(&mut bytes, 3); // version
  push_u8(&mut bytes, 0); // reserved0
  push_u16(&mut bytes, 0); // reserved1
  push_u32(&mut bytes, 1); // numFunctions
  push_u32(&mut bytes, 0); // numConstants
  push_u32(&mut bytes, 1); // numRecords

  // Function record.
  push_u64(&mut bytes, 0); // address
  push_u64(&mut bytes, 24); // stack_size
  push_u64(&mut bytes, 1); // record_count

  // Record header.
  push_u64(&mut bytes, 0); // patchpoint_id
  push_u32(&mut bytes, instruction_offset);
  push_u16(&mut bytes, 0); // reserved
  push_u16(&mut bytes, 5); // num_locations

  // Helper: StackMap location entry (12 bytes).
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset: i32) {
    out.push(kind);
    out.push(0); // reserved0
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&dwarf_reg.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    out.extend_from_slice(&offset.to_le_bytes());
  }

  // 3 constant header locations (calling convention, flags, deopt count).
  push_loc(&mut bytes, 4, 8, 0, 0);
  push_loc(&mut bytes, 4, 8, 0, 0);
  push_loc(&mut bytes, 4, 8, 0, 0);

  // One (base, derived) pair: Indirect [RSP+0], Indirect [RSP+8].
  push_loc(&mut bytes, 3, 8, 7, 0);
  push_loc(&mut bytes, 3, 8, 7, 8);

  // Align to 8 before live-out header.
  align_to(&mut bytes, 8);
  push_u16(&mut bytes, 0); // live-out padding
  push_u16(&mut bytes, 0); // num_live_outs
  // No live outs.
  align_to(&mut bytes, 8);

  bytes
}
