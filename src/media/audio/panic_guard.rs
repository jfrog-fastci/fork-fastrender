use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Audio sample types that can represent "silence".
///
/// CPAL supports multiple sample formats. Signed integer and float formats use `0` for silence;
/// unsigned integer formats use the mid-point ("equilibrium").
pub(crate) trait AudioSample: Copy {
  const SILENCE: Self;
}

macro_rules! impl_audio_sample_zero {
  ($($t:ty),* $(,)?) => {
    $(
      impl AudioSample for $t {
        const SILENCE: Self = 0 as $t;
      }
    )*
  };
}

macro_rules! impl_audio_sample_equilibrium {
  ($($t:ty),* $(,)?) => {
    $(
      impl AudioSample for $t {
        const SILENCE: Self = (1 as $t) << (<$t>::BITS - 1);
      }
    )*
  };
}

impl_audio_sample_zero!(i8, i16, i32, i64, f32, f64);
impl_audio_sample_equilibrium!(u8, u16, u32, u64);

/// Lock a mutex in a way that tolerates poisoning.
///
/// If the real-time audio callback panics, any locks held during the panic become poisoned. We
/// still want the callback to be able to acquire the lock on subsequent invocations in order to
/// output silence or allow the management thread to recover.
pub(crate) fn lock_ignore_poison<'a, T>(mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
  mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Execute a real-time audio callback body with panic containment.
///
/// Returns `true` if `f` panicked. In that case, this helper:
/// - fills `output` with silence for this callback invocation, and
/// - sets `callback_panicked` to `true` (sticky flag for telemetry / recovery logic).
///
/// This is intended to wrap CPAL's output callback, which must never unwind across the callback
/// boundary (FFI) because it may lead to undefined behaviour or process aborts.
pub(crate) fn guard_output_callback<T: AudioSample>(
  output: &mut [T],
  callback_panicked: &AtomicBool,
  f: impl FnOnce(&mut [T]),
) -> bool {
  if catch_unwind(AssertUnwindSafe(|| f(output))).is_ok() {
    return false;
  }

  callback_panicked.store(true, Ordering::Relaxed);
  output.fill(T::SILENCE);
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn panicking_callback_outputs_silence_and_sets_flag() {
    let callback_panicked = AtomicBool::new(false);
    let mut out = [1.0_f32, 2.0, 3.0, 4.0];

    let did_panic = guard_output_callback(&mut out, &callback_panicked, |_out| {
      panic!("boom");
    });

    assert!(did_panic);
    assert!(callback_panicked.load(Ordering::Relaxed));
    assert_eq!(out, [0.0, 0.0, 0.0, 0.0]);
  }

  #[test]
  fn non_panicking_callback_preserves_output() {
    let callback_panicked = AtomicBool::new(false);
    let mut out = [0.0_f32; 4];

    let did_panic = guard_output_callback(&mut out, &callback_panicked, |out| {
      out.copy_from_slice(&[0.25, 0.5, 0.75, 1.0]);
    });

    assert!(!did_panic);
    assert!(!callback_panicked.load(Ordering::Relaxed));
    assert_eq!(out, [0.25, 0.5, 0.75, 1.0]);
  }

  #[test]
  fn panicking_callback_outputs_equilibrium_for_unsigned() {
    let callback_panicked = AtomicBool::new(false);
    let mut out = [1_u16, 2, 3, 4];

    let did_panic = guard_output_callback(&mut out, &callback_panicked, |_out| {
      panic!("boom");
    });

    assert!(did_panic);
    assert!(callback_panicked.load(Ordering::Relaxed));
    assert_eq!(<u16 as AudioSample>::SILENCE, 1 << 15);
    assert_eq!(out, [<u16 as AudioSample>::SILENCE; 4]);
  }

  #[test]
  fn lock_ignore_poison_recovers_poisoned_mutex() {
    let mutex = Mutex::new(0u32);

    let _ = catch_unwind(AssertUnwindSafe(|| {
      let mut guard = mutex.lock().unwrap();
      *guard = 123;
      panic!("poison");
    }));

    assert!(mutex.lock().is_err(), "mutex should be poisoned after panic");
    let guard = lock_ignore_poison(&mutex);
    assert_eq!(*guard, 123);
  }
}
