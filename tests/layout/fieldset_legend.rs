use fastrender::api::LayoutIntermediates;
use fastrender::debug::inspect::{inspect, InspectionSnapshot, RectSnapshot};
use fastrender::style::media::MediaType;
use fastrender::{FastRender, InspectQuery};

fn block_bounds(snapshot: &InspectionSnapshot) -> RectSnapshot {
  snapshot
    .fragments
    .iter()
    .find(|f| f.kind == "block")
    .map(|f| f.bounds.clone())
    .expect("expected a block fragment")
}

fn layout_intermediates(
  renderer: &mut FastRender,
  html: &str,
  viewport_width: u32,
  viewport_height: u32,
) -> LayoutIntermediates {
  let dom = renderer.parse_html(html).expect("parse");
  renderer
    .layout_document_for_media_intermediates(&dom, viewport_width, viewport_height, MediaType::Screen)
    .expect("layout intermediates")
}

fn inspect_id(intermediates: &LayoutIntermediates, id: &str) -> InspectionSnapshot {
  let mut results = inspect(
    &intermediates.dom,
    &intermediates.styled_tree,
    &intermediates.box_tree.root,
    &intermediates.fragment_tree,
    InspectQuery::Id(id.to_string()),
  )
  .expect("inspect");
  assert_eq!(results.len(), 1, "expected exactly one match for #{id}");
  results.remove(0)
}

#[test]
fn fieldset_legend_overlaps_border_and_pushes_content_down() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      fieldset#fs { margin: 0; padding: 0; border: 4px solid black; }
      legend#lg { margin: 0; padding: 0 20px; }
      #c { display: block; margin: 0; height: 10px; }
    </style>
    <fieldset id="fs">
      <legend id="lg">Legend</legend>
      <div id="c"></div>
    </fieldset>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let intermediates = layout_intermediates(&mut renderer, html, 200, 100);
  let fs = inspect_id(&intermediates, "fs");
  let lg = inspect_id(&intermediates, "lg");
  let c = inspect_id(&intermediates, "c");

  let fs_bounds = block_bounds(&fs);
  let lg_bounds = block_bounds(&lg);
  let c_bounds = block_bounds(&c);

  // The legend should be pulled upward to overlap the border-top line.
  assert!(
    lg_bounds.y < fs_bounds.y + 4.5,
    "legend should overlap fieldset border-top (fieldset_y={} legend_y={})",
    fs_bounds.y,
    lg_bounds.y
  );

  // The content should start below the legend's bottom edge to avoid overlap.
  assert!(
    c_bounds.y + 0.1 >= lg_bounds.y + lg_bounds.height,
    "content should start below legend bottom (legend_bottom={} content_y={})",
    lg_bounds.y + lg_bounds.height,
    c_bounds.y
  );
}

#[test]
fn fieldset_legend_overlaps_border_and_pushes_content_down_vertical_rl() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      fieldset#fs {
        margin: 0;
        padding: 0;
        width: 40px;
        height: 200px;
        border: 4px solid black;
        writing-mode: vertical-rl;
      }
      legend#lg { margin: 0; padding: 40px 0; }
      #c { display: block; margin: 0; width: 10px; height: 10px; }
    </style>
    <fieldset id="fs">
      <legend id="lg">Legend</legend>
      <div id="c"></div>
    </fieldset>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let intermediates = layout_intermediates(&mut renderer, html, 80, 240);
  let fs = inspect_id(&intermediates, "fs");
  let lg = inspect_id(&intermediates, "lg");
  let c = inspect_id(&intermediates, "c");

  let fs_bounds = block_bounds(&fs);
  let lg_bounds = block_bounds(&lg);
  let c_bounds = block_bounds(&c);

  // In vertical-rl, the block-start edge is on the physical right.
  // The legend should be pulled toward the right border so it overlaps that border line.
  let fs_right = fs_bounds.x + fs_bounds.width;
  let lg_right = lg_bounds.x + lg_bounds.width;
  assert!(
    lg_right > fs_right - 4.5,
    "legend should overlap fieldset border-start (right) edge (fieldset_right={} legend_right={})",
    fs_right,
    lg_right
  );

  // The content should start to the left of the legend's left edge to avoid overlap.
  let c_right = c_bounds.x + c_bounds.width;
  assert!(
    c_right <= lg_bounds.x + 0.1,
    "content should start after legend block-end (legend_left={} content_right={})",
    lg_bounds.x,
    c_right
  );
}

#[test]
fn legend_shrinks_to_fit_in_fieldset() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      fieldset#fs { width: 200px; margin: 0; padding: 0; border: 1px solid black; }
      legend#lg { margin: 0; padding: 0; }
    </style>
    <fieldset id="fs">
      <legend id="lg">Legend</legend>
    </fieldset>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let intermediates = layout_intermediates(&mut renderer, html, 240, 80);
  let fs = inspect_id(&intermediates, "fs");
  let lg = inspect_id(&intermediates, "lg");

  let fs_bounds = block_bounds(&fs);
  let lg_bounds = block_bounds(&lg);

  assert!(
    lg_bounds.width < fs_bounds.width * 0.75,
    "legend should shrink-to-fit instead of filling the fieldset width (fieldset_width={} legend_width={})",
    fs_bounds.width,
    lg_bounds.width
  );
}

#[test]
fn legend_shrinks_to_fit_in_fieldset_vertical_rl() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      fieldset#fs {
        width: 40px;
        height: 200px;
        margin: 0;
        padding: 0;
        border: 1px solid black;
        writing-mode: vertical-rl;
      }
      legend#lg { margin: 0; padding: 0; }
    </style>
    <fieldset id="fs">
      <legend id="lg">Legend</legend>
    </fieldset>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let intermediates = layout_intermediates(&mut renderer, html, 80, 240);
  let fs = inspect_id(&intermediates, "fs");
  let lg = inspect_id(&intermediates, "lg");

  let fs_bounds = block_bounds(&fs);
  let lg_bounds = block_bounds(&lg);

  assert!(
    lg_bounds.height < fs_bounds.height * 0.75,
    "legend should shrink-to-fit instead of filling the fieldset inline size (fieldset_height={} legend_height={})",
    fs_bounds.height,
    lg_bounds.height
  );
}

#[test]
fn fieldset_without_legend_does_not_introduce_extra_offset() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      fieldset#fs { margin: 0; padding: 6px; border: 4px solid black; }
      #c { display: block; margin: 0; height: 10px; }
    </style>
    <fieldset id="fs">
      <div id="c"></div>
    </fieldset>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let intermediates = layout_intermediates(&mut renderer, html, 200, 120);
  let fs = inspect_id(&intermediates, "fs");
  let c = inspect_id(&intermediates, "c");

  let fs_bounds = block_bounds(&fs);
  let c_bounds = block_bounds(&c);

  let expected = 4.0 + 6.0;
  let actual = c_bounds.y - fs_bounds.y;
  assert!(
    (actual - expected).abs() < 0.5,
    "content should start at border+padding without a legend (expected={} actual={})",
    expected,
    actual
  );
}

#[test]
fn fieldset_without_legend_does_not_introduce_extra_offset_vertical_rl() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      fieldset#fs {
        margin: 0;
        width: 120px;
        height: 200px;
        writing-mode: vertical-rl;
        padding: 0;
        border: 0;
        border-top: 2px solid black;
        border-right: 8px solid black;
        padding-top: 3px;
        padding-right: 7px;
      }
      #c { display: block; margin: 0; width: 10px; height: 10px; }
    </style>
    <fieldset id="fs">
      <div id="c"></div>
    </fieldset>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let intermediates = layout_intermediates(&mut renderer, html, 200, 240);
  let fs = inspect_id(&intermediates, "fs");
  let c = inspect_id(&intermediates, "c");

  let fs_bounds = block_bounds(&fs);
  let c_bounds = block_bounds(&c);

  // Inline-start in vertical-rl is the physical top edge.
  let expected_inline = 2.0 + 3.0;
  let actual_inline = c_bounds.y - fs_bounds.y;
  assert!(
    (actual_inline - expected_inline).abs() < 0.5,
    "content inline-start should offset by border-top+padding-top (expected={} actual={})",
    expected_inline,
    actual_inline
  );

  // Block-start in vertical-rl is the physical right edge.
  let expected_block = 8.0 + 7.0;
  let fs_right = fs_bounds.x + fs_bounds.width;
  let c_right = c_bounds.x + c_bounds.width;
  let actual_block = fs_right - c_right;
  assert!(
    (actual_block - expected_block).abs() < 0.5,
    "content block-start should offset by border-right+padding-right (expected={} actual={})",
    expected_block,
    actual_block
  );
}
