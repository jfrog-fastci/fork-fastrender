use base64::{engine::general_purpose::STANDARD, Engine as _};
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list::ResolvedFilter;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::svg_filter::{ColorInterpolationFilters, FilterInput, FilterPrimitive, TransferFn};
use fastrender::FastRender;

#[test]
fn filter_url_data_resolves_to_svg_filter() {
  let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='2' height='2'><filter id='f'><feFlood flood-color='rgb(255,0,0)' flood-opacity='1' result='f'/><feComposite in='f' in2='SourceAlpha' operator='in'/></filter></svg>";
  let data_url = format!(
    "data:image/svg+xml;base64,{}#f",
    STANDARD.encode(svg.as_bytes())
  );
  let html = format!(
    "<style>body {{ margin: 0; }} #target {{ width: 4px; height: 4px; background: rgb(0, 0, 255); filter: url(\"{}\"); }}</style><div id=\"target\"></div>",
    data_url
  );

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 10, 10)
    .expect("layout document");

  let list = DisplayListBuilder::new().build_with_stacking_tree(&fragments.root);

  let has_svg_filter = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) => Some(
        ctx
          .filters
          .iter()
          .any(|filter| matches!(filter, ResolvedFilter::SvgFilter(_))),
      ),
      _ => None,
    })
    .any(|v| v);

  assert!(
    has_svg_filter,
    "stacking context filters should include SVG filters resolved from url()"
  );
}

#[test]
fn filter_url_data_with_quotes_resolves_to_svg_filter() {
  let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='2' height='2'><filter id='f'><feFlood flood-color='rgb(255,0,0)' flood-opacity='1' result='f'/><feComposite in='f' in2='SourceAlpha' operator='in'/></filter></svg>";
  let data_url = format!(
    "data:image/svg+xml;base64,{}#f",
    STANDARD.encode(svg.as_bytes())
  );
  let html = format!(
    "<style>body {{ margin: 0; }} #target {{ width: 4px; height: 4px; background: rgb(0, 0, 255); filter: url('{data_url}'); }}</style><div id=\"target\"></div>",
    data_url = data_url
  );

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 10, 10)
    .expect("layout document");

  let list = DisplayListBuilder::new().build_with_stacking_tree(&fragments.root);

  let has_svg_filter = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) => Some(
        ctx
          .filters
          .iter()
          .any(|filter| matches!(filter, ResolvedFilter::SvgFilter(_))),
      ),
      _ => None,
    })
    .any(|v| v);

  assert!(
    has_svg_filter,
    "stacking context filters should include SVG filters resolved from url() with quotes"
  );
}

#[test]
fn filter_url_data_component_transfer_invert_srgb_resolves_to_svg_filter() {
  // Matches the Wikipedia pageset pattern: `filter:url(data:image/svg+xml...#filter)` where the
  // embedded SVG document defines a single `feComponentTransfer` invert filter with
  // `color-interpolation-filters="sRGB"` on the primitive (not the parent `<filter>` element).
  let svg = "<svg xmlns='http://www.w3.org/2000/svg'><filter id='filter'><feComponentTransfer color-interpolation-filters='sRGB'><feFuncR type='table' tableValues='1 0'/><feFuncG type='table' tableValues='1 0'/><feFuncB type='table' tableValues='1 0'/></feComponentTransfer></filter></svg>";
  let data_url = format!(
    "data:image/svg+xml;base64,{}#filter",
    STANDARD.encode(svg.as_bytes())
  );
  let html = format!(
    "<style>body {{ margin: 0; }} #target {{ width: 4px; height: 4px; background: rgb(0, 0, 255); filter: url(\"{}\"); }}</style><div id=\"target\"></div>",
    data_url
  );

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 10, 10)
    .expect("layout document");

  let list = DisplayListBuilder::new().build_with_stacking_tree(&fragments.root);

  let svg_filter = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) => ctx.filters.iter().find_map(|filter| match filter {
        ResolvedFilter::SvgFilter(filter) => Some(filter.clone()),
        _ => None,
      }),
      _ => None,
    })
    .next()
    .expect("expected SvgFilter to be present in stacking context filters");

  assert_eq!(svg_filter.steps.len(), 1);
  assert_eq!(svg_filter.color_interpolation_filters, ColorInterpolationFilters::LinearRGB);

  let step = &svg_filter.steps[0];
  assert_eq!(step.color_interpolation_filters, Some(ColorInterpolationFilters::SRGB));

  match &step.primitive {
    FilterPrimitive::ComponentTransfer { input, r, g, b, a } => {
      assert!(
        matches!(input, FilterInput::Previous),
        "expected component transfer to default to previous input, got {:?}",
        input
      );
      for tf in [r, g, b] {
        match tf {
          TransferFn::Table { values } => assert_eq!(values.as_slice(), &[1.0, 0.0]),
          other => panic!("expected TransferFn::Table for invert transfer, got {:?}", other),
        }
      }
      assert!(
        matches!(a, TransferFn::Identity),
        "expected alpha transfer to default to identity, got {:?}",
        a
      );
    }
    other => panic!("expected component transfer primitive, got {:?}", other),
  }
}
