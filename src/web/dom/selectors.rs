use crate::css::selectors::{
  namespace_context_set_default, namespace_context_set_prefix, FastRenderSelectorImpl,
  NamespaceContextGuard, PseudoClassParser,
};
use crate::css::types::CssString;
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

pub fn parse_selector_list_for_dom(
  dom_is_html: bool,
  default_ns: Option<&str>,
  prefixes: &[(String, String)],
  selector: &str,
) -> Result<SelectorList<FastRenderSelectorImpl>, DomException> {
  let _ns = NamespaceContextGuard::new();
  if dom_is_html {
    // HTML documents do not expose a namespace prefix map to selector parsing for DOM APIs.
    //
    // Keep the namespace context empty (no default namespace, no prefixes), matching historical
    // FastRender behavior and ensuring unprefixed type selectors can match non-HTML namespaces (e.g.
    // `<svg>` in an HTML document).
  } else {
    if let Some(default_ns) = default_ns {
      namespace_context_set_default(CssString::from(default_ns));
    }
    for (prefix, url) in prefixes {
      namespace_context_set_prefix(prefix, CssString::from(url.as_str()));
    }
  }

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
