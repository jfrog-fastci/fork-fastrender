use crate::css::selectors::{FastRenderSelectorImpl, PseudoClassParser};
use crate::dom::{DomNode, ElementRef};
use crate::web::dom::DomException;
use cssparser::{Parser, ParserInput};
use selectors::context::QuirksMode;
use selectors::matching::{
  matches_selector, MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags,
  SelectorCaches,
};
use selectors::parser::{ParseRelative, SelectorList};
use selectors::OpaqueElement;

pub fn parse_selector_list(
  selector: &str,
) -> Result<SelectorList<FastRenderSelectorImpl>, DomException> {
  let mut input = ParserInput::new(selector);
  let mut parser = Parser::new(&mut input);
  SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
    .map_err(|e| DomException::syntax_error(format!("Invalid selector {selector:?}: {e:?}")))
}

pub fn node_matches_selector_list(
  node: &DomNode,
  ancestors: &[&DomNode],
  selectors: &SelectorList<FastRenderSelectorImpl>,
  caches: &mut SelectorCaches,
  quirks_mode: QuirksMode,
  scope_anchor: Option<OpaqueElement>,
) -> bool {
  if !node.is_element() {
    return false;
  }

  let element_ref = ElementRef::with_ancestors(node, ancestors);
  let mut context = MatchingContext::new(
    MatchingMode::Normal,
    None,
    caches,
    quirks_mode,
    NeedsSelectorFlags::No,
    MatchingForInvalidation::No,
  );

  let matches = |ctx: &mut MatchingContext<'_, FastRenderSelectorImpl>| {
    selectors
      .slice()
      .iter()
      .any(|sel| matches_selector(sel, 0, None, &element_ref, ctx))
  };

  match scope_anchor {
    Some(scope) => context.nest_for_scope(Some(scope), matches),
    None => matches(&mut context),
  }
}
