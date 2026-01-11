use runtime_native::roots::{conservative_scan_words, HeapRange};

#[test]
fn conservative_scan_reports_only_in_heap_aligned_non_null_words() {
  let heap: Box<[u8; 256]> = Box::new([0; 256]);
  let heap_start = heap.as_ptr();
  let heap_end = unsafe { heap_start.add(heap.len()) };
  let heap_range = HeapRange::new(heap_start, heap_end);

  let align = core::mem::align_of::<usize>();
  let start_addr = heap_start as usize;
  let first_aligned_in_heap = (start_addr + (align - 1)) & !(align - 1);
  let in_heap_aligned = first_aligned_in_heap as usize;
  assert!(heap_range.contains(in_heap_aligned as *const u8));

  let in_heap_misaligned = in_heap_aligned + 1;
  assert!(heap_range.contains(in_heap_misaligned as *const u8));

  let outside_heap = (&heap_range as *const _ as usize) & !(align - 1);
  assert!(!heap_range.contains(outside_heap as *const u8));

  let at_end = heap_end as usize;

  let words: [usize; 7] = [
    0,                // null
    in_heap_aligned,  // should be reported
    outside_heap,     // outside heap
    in_heap_misaligned, // misaligned
    at_end,           // end-exclusive
    in_heap_aligned,  // duplicate should be reported again
    1,                // non-null but misaligned
  ];

  let mut found = Vec::new();
  let range = words.as_ptr()..unsafe { words.as_ptr().add(words.len()) };
  conservative_scan_words(range, heap_range, |slot| unsafe {
    found.push(*(slot as *mut usize));
  });

  assert_eq!(found, vec![in_heap_aligned, in_heap_aligned]);
}

#[test]
fn conservative_scan_fuzz_no_panics_and_only_reports_valid_candidates() {
  let heap: Box<[u8; 512]> = Box::new([0; 512]);
  let heap_start = heap.as_ptr();
  let heap_end = unsafe { heap_start.add(heap.len()) };
  let heap_range = HeapRange::new(heap_start, heap_end);

  let align = core::mem::align_of::<usize>();
  let start_addr = heap_start as usize;
  let first_aligned_in_heap = (start_addr + (align - 1)) & !(align - 1);
  let in_heap_aligned = first_aligned_in_heap as usize;
  assert!(heap_range.contains(in_heap_aligned as *const u8));

  let mut seed: u64 = 0x1234_5678_9abc_def0;
  for _ in 0..200 {
    // xorshift64*
    seed ^= seed >> 12;
    seed ^= seed << 25;
    seed ^= seed >> 27;
    seed = seed.wrapping_mul(0x2545_f491_4f6c_dd1d);

    let len = (seed as usize % 32) + 1;
    let mut words = vec![0usize; len];
    for word in &mut words {
      // Re-mix each element.
      seed ^= seed >> 12;
      seed ^= seed << 25;
      seed ^= seed >> 27;
      seed = seed.wrapping_mul(0x2545_f491_4f6c_dd1d);
      *word = seed as usize;
    }

    // Ensure at least one real in-heap aligned candidate.
    words[seed as usize % len] = in_heap_aligned;

    let range = words.as_ptr()..unsafe { words.as_ptr().add(words.len()) };
    conservative_scan_words(range, heap_range, |slot| unsafe {
      let word = *(slot as *mut usize);
      assert_ne!(word, 0);
      assert_eq!(word % align, 0);
      assert!(heap_range.contains(word as *const u8));
    });
  }
}

