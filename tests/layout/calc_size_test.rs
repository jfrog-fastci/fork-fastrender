use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::properties::apply_declaration;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn calc_size_auto_resolves_against_available_width() {
  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(10.0));
  child_style.height_keyword = None;

  let decl = Declaration {
    property: "width".into(),
    value: PropertyValue::Keyword("calc-size(auto, size - 10px)".into()),
    raw_value: String::new(),
    important: false,
    contains_var: false,
  };
  apply_declaration(&mut child_style, &decl, &ComputedStyle::default(), 16.0, 16.0);

  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    Vec::new(),
  );
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![child],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(
      &root,
      &LayoutConstraints::new(
        AvailableSpace::Definite(200.0),
        AvailableSpace::Definite(100.0),
      ),
    )
    .expect("layout");
  let child_fragment = fragment.children.first().expect("child fragment");
  assert!(
    (child_fragment.bounds.width() - 190.0).abs() < 0.5,
    "expected calc-size() to reduce used width to ≈190px, got {:.2}",
    child_fragment.bounds.width()
  );
}

