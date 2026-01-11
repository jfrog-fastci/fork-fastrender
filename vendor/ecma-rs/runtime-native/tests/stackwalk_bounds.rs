use runtime_native::stackwalk::StackBounds;
use runtime_native::stackwalk::StackWalkError;
use runtime_native::stackwalk::StackWalker;
use runtime_native::stackwalk::ThreadContext;

#[repr(align(16))]
struct AlignedStack<const N: usize>([u8; N]);

unsafe fn write_u64(addr: u64, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn valid_chain_visits_expected_frames() {
  let mut mem = AlignedStack([0u8; 256]);
  let base = mem.0.as_mut_ptr() as u64;
  let hi = base + mem.0.len() as u64;

  let fp0 = base + 0x20;
  let fp1 = base + 0x40;
  let fp2 = base + 0x60;
  assert_eq!(fp0 % 16, 0);
  assert_eq!(fp1 % 16, 0);
  assert_eq!(fp2 % 16, 0);

  unsafe {
    write_u64(fp0, fp1);
    write_u64(fp0 + 8, 0x1111);

    write_u64(fp1, fp2);
    write_u64(fp1 + 8, 0x2222);

    write_u64(fp2, 0);
    write_u64(fp2 + 8, 0x3333);
  }

  let ctx = ThreadContext::new(0, fp0, 0);
  let bounds = StackBounds::new(base, hi).unwrap();
  let frames: Vec<u64> = StackWalker::new(ctx, bounds)
    .unwrap()
    .map(Result::unwrap)
    .map(|f| f.return_address)
    .collect();

  assert_eq!(frames, vec![0x1111, 0x2222, 0x3333]);
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn corrupted_fp_chain_stops_safely() {
  let mut mem = AlignedStack([0u8; 128]);
  let base = mem.0.as_mut_ptr() as u64;
  let hi = base + mem.0.len() as u64;

  let fp0 = base + 0x20;
  unsafe {
    write_u64(fp0, fp0);
    write_u64(fp0 + 8, 0x1111);
  }

  let ctx = ThreadContext::new(0, fp0, 0);
  let bounds = StackBounds::new(base, hi).unwrap();
  let mut walker = StackWalker::new(ctx, bounds).unwrap();
  let err = walker.next().unwrap().unwrap_err();
  assert!(matches!(
    err,
    StackWalkError::NonMonotonicFramePointer { .. }
  ));
  assert!(walker.next().is_none());
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn out_of_bounds_fp_stops_safely() {
  let mut mem = AlignedStack([0u8; 64]);
  let base = mem.0.as_mut_ptr() as u64;
  let hi = base + mem.0.len() as u64;

  let fp = hi;
  assert_eq!(fp % 16, 0);

  let ctx = ThreadContext::new(0, fp, 0);
  let bounds = StackBounds::new(base, hi).unwrap();
  let mut walker = StackWalker::new(ctx, bounds).unwrap();

  let err = walker.next().unwrap().unwrap_err();
  assert!(matches!(err, StackWalkError::FramePointerOutOfBounds { .. }));
  assert!(walker.next().is_none());
}
