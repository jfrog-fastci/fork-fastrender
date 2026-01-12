use super::compare::{compare_images, load_png, CompareConfig};
use super::harness::{RefTestConfig, RefTestHarness};
use super::test_utils::with_large_stack;
use std::path::Path;

#[test]
fn form_controls_reference_image_matches_golden() {
  with_large_stack(|| {
    let fixture = Path::new("tests/ref/fixtures/form_controls");
    let reference = fixture.join("reference.png");
    let mut harness = RefTestHarness::new();

    if std::env::var("UPDATE_GOLDEN").is_ok() {
      let html = std::fs::read_to_string(fixture.join("input.html")).expect("read html");
      harness
        .create_reference(&html, &reference)
        .expect("create reference image");
    }

    let result = harness.run_ref_test(fixture, &reference);
    if !result.passed {
      let actual_path = fixture.join("failures/form_controls_actual.png");
      if let (Ok(actual), Ok(expected)) = (load_png(&actual_path), load_png(&reference)) {
        let diff = compare_images(&actual, &expected, &CompareConfig::strict());
        if diff.is_match() {
          let _ = std::fs::remove_dir_all(fixture.join("failures"));
          return;
        }
      }
    }
    assert!(
      result.passed,
      "form control rendering regressed: {}",
      result.summary()
    );
  });
}

#[test]
fn input_button_text_align_reference_image_matches_golden() {
  with_large_stack(|| {
    let fixture = Path::new("tests/ref/fixtures/input_button_text_align");
    let reference = fixture.join("reference.png");
    let mut harness = RefTestHarness::with_config(RefTestConfig::with_viewport(340, 80));

    if std::env::var("UPDATE_GOLDEN").is_ok() {
      let html = std::fs::read_to_string(fixture.join("input.html")).expect("read html");
      harness
        .create_reference(&html, &reference)
        .expect("create reference image");
    }

    let result = harness.run_ref_test(fixture, &reference);
    if !result.passed {
      let actual_path = fixture.join("failures/input_button_text_align_actual.png");
      if let (Ok(actual), Ok(expected)) = (load_png(&actual_path), load_png(&reference)) {
        let diff = compare_images(&actual, &expected, &CompareConfig::strict());
        if diff.is_match() {
          let _ = std::fs::remove_dir_all(fixture.join("failures"));
          return;
        }
      }
    }
    assert!(
      result.passed,
      "input button text-align rendering regressed: {}",
      result.summary()
    );
  });
}

#[test]
fn form_controls_rtl_mirror_reference_image_matches_golden() {
  with_large_stack(|| {
    let fixture = Path::new("tests/ref/fixtures/form_controls_rtl_mirror");
    let reference = fixture.join("reference.png");
    let mut harness = RefTestHarness::new();

    if std::env::var("UPDATE_GOLDEN").is_ok() {
      let html = std::fs::read_to_string(fixture.join("input.html")).expect("read html");
      harness
        .create_reference(&html, &reference)
        .expect("create reference image");
    }

    let result = harness.run_ref_test(fixture, &reference);
    if !result.passed {
      let actual_path = fixture.join("failures/form_controls_rtl_mirror_actual.png");
      if let (Ok(actual), Ok(expected)) = (load_png(&actual_path), load_png(&reference)) {
        let diff = compare_images(&actual, &expected, &CompareConfig::strict());
        if diff.is_match() {
          let _ = std::fs::remove_dir_all(fixture.join("failures"));
          return;
        }
      }
    }
    assert!(
      result.passed,
      "form control RTL mirroring regressed: {}",
      result.summary()
    );
  });
}

