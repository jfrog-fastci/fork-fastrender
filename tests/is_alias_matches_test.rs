use cssparser::{Parser, ParserInput};
use fastrender::css::selectors::{FastRenderSelectorImpl, PseudoClassParser, ShadowMatchData};
use fastrender::dom::{
  next_selector_cache_epoch, DomNode, DomNodeType, ElementRef, SiblingListCache, HTML_NAMESPACE,
};
use selectors::context::QuirksMode;
use selectors::matching::{
  matches_selector, MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags,
  SelectorCaches,
};
use selectors::parser::{ParseRelative, Selector, SelectorList};

fn parse_selector(selector: &str) -> Selector<FastRenderSelectorImpl> {
  let mut input = ParserInput::new(selector);
  let mut parser = Parser::new(&mut input);
  SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
    .expect("selector should parse")
    .slice()
    .first()
    .expect("selector list should have at least one selector")
    .clone()
}

fn selector_matches(element: &ElementRef, selector: &Selector<FastRenderSelectorImpl>) -> bool {
  let mut caches = SelectorCaches::default();
  let cache_epoch = next_selector_cache_epoch();
  caches.set_epoch(cache_epoch);
  let sibling_cache = SiblingListCache::new(cache_epoch);
  let mut context = MatchingContext::new(
    MatchingMode::Normal,
    None,
    &mut caches,
    QuirksMode::NoQuirks,
    NeedsSelectorFlags::No,
    MatchingForInvalidation::No,
  );
  context.extra_data = ShadowMatchData::for_document().with_sibling_cache(&sibling_cache);
  matches_selector(selector, 0, None, element, &mut context)
}

fn element_with_class(tag: &str, class: Option<&str>, children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: tag.to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: class
        .map(|value| vec![("class".to_string(), value.to_string())])
        .unwrap_or_default(),
    },
    children,
  }
}

#[test]
fn matches_is_accepted_as_is_alias() {
  let selector = parse_selector("div:matches(.foo)");

  let dom = element_with_class("div", Some("foo"), vec![]);
  let element_ref = ElementRef::with_ancestors(&dom, &[]);
  assert!(selector_matches(&element_ref, &selector));
}

#[test]
fn matches_alias_is_forgiving_like_is() {
  // `:is()` uses forgiving selector lists; `:matches()` is a legacy alias and should behave the
  // same. The invalid selector is dropped, leaving `.foo`.
  let selector = parse_selector("div:matches(.foo, ::before)");

  let dom = element_with_class("div", Some("foo"), vec![]);
  let element_ref = ElementRef::with_ancestors(&dom, &[]);
  assert!(selector_matches(&element_ref, &selector));
}

#[test]
fn host_context_requires_compound_selector() {
  // Per CSS Scoping, :host-context() takes a single <compound-selector>. It must not accept
  // selector lists, combinators, or pseudo-elements.
  for selector in [
    "div:host-context(.foo, .bar)",
    "div:host-context(.foo .bar)",
    "div:host-context(::before)",
  ] {
    let mut input = ParserInput::new(selector);
    let mut parser = Parser::new(&mut input);
    assert!(
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_err(),
      "expected parse error for {selector}"
    );
  }
}

#[test]
fn invalid_host_context_is_dropped_inside_is() {
  // :is() is forgiving; an invalid :host-context() argument should make that branch invalid and
  // dropped, leaving `.baz`.
  let selector = parse_selector("div:is(:host-context(.foo, .bar), .baz)");

  let dom = element_with_class("div", Some("baz"), vec![]);
  let element_ref = ElementRef::with_ancestors(&dom, &[]);
  assert!(selector_matches(&element_ref, &selector));
}
