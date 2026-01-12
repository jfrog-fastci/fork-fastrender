/// Compatibility shim.
///
/// Prefer calling [`crate::common::init_rayon_for_tests`] (or
/// [`crate::common::rayon::init_rayon_for_tests`]) directly in new tests.
pub fn init_rayon_for_tests(num_threads: usize) {
  super::rayon::init_rayon_for_tests(num_threads);
}
