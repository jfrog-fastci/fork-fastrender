use crate::image_loader::ImageCache;
use crate::paint::svg_filter::{parse_svg_filter_from_svg_document, FilterPrimitive, TransferFn};

#[test]
fn convolve_matrix_massive_order_is_rejected_without_panic() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">
      <defs>
        <filter id="f">
          <feFlood flood-color="red" />
          <feConvolveMatrix order="1000000000" kernelMatrix="1" />
        </filter>
      </defs>
    </svg>
  "#;

  let cache = ImageCache::new();
  let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    parse_svg_filter_from_svg_document(svg, Some("f"), &cache)
  }));
  assert!(parsed.is_ok(), "SVG filter parse panicked");

  let filter = parsed
    .unwrap()
    .expect("expected filter to parse (feFlood should remain)");

  // Oversized feConvolveMatrix should be rejected and not added to the filter graph.
  assert_eq!(
    filter.steps.len(),
    1,
    "expected only the safe primitives to be kept"
  );
  assert!(matches!(
    filter.steps[0].primitive,
    FilterPrimitive::Flood { .. }
  ));
}

#[test]
fn component_transfer_table_values_is_capped() {
  // Build a huge tableValues list; the parser should cap it to a safe limit.
  let mut table_values = String::new();
  // 5000 entries is large enough to exceed the cap while remaining lightweight for the test.
  for _ in 0..5000 {
    table_values.push_str("0 ");
  }

  let svg = format!(
    r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">
      <defs>
        <filter id="f">
          <feComponentTransfer in="SourceGraphic">
            <feFuncR type="table" tableValues="{table_values}" />
          </feComponentTransfer>
        </filter>
      </defs>
    </svg>
  "#
  );

  let cache = ImageCache::new();
  let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    parse_svg_filter_from_svg_document(&svg, Some("f"), &cache)
  }));
  assert!(parsed.is_ok(), "SVG filter parse panicked");

  let filter = parsed.unwrap().expect("expected filter to parse");
  assert_eq!(filter.steps.len(), 1);

  match &filter.steps[0].primitive {
    FilterPrimitive::ComponentTransfer { r, .. } => match r {
      TransferFn::Table { values } => {
        assert_eq!(
          values.len(),
          super::super::MAX_COMPONENT_TRANSFER_TABLE_VALUES,
          "expected tableValues to be capped"
        );
      }
      other => panic!("expected feFuncR to parse as TransferFn::Table, got {other:?}"),
    },
    other => panic!("expected ComponentTransfer primitive, got {other:?}"),
  }
}
