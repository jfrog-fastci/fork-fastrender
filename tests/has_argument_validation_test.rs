use cssparser::{Parser, ParserInput};
use fastrender::css::selectors::{FastRenderSelectorImpl, PseudoClassParser, ShadowMatchData};
use fastrender::dom::{next_selector_cache_epoch, DomNode, DomNodeType, ElementRef, SiblingListCache, HTML_NAMESPACE};
use selectors::context::QuirksMode;
use selectors::matching::{matches_selector, MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, SelectorCaches};
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
fn has_disallows_nested_has() {
  let mut input = ParserInput::new("div:has(:has(.foo))");
  let mut parser = Parser::new(&mut input);
  assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_err());
}

#[test]
fn has_disallows_pseudo_elements_in_argument() {
  let mut input = ParserInput::new("div:has(::before)");
  let mut parser = Parser::new(&mut input);
  assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_err());
}

#[test]
fn has_argument_list_is_not_forgiving() {
  let mut input = ParserInput::new("div:has(.foo, ::before)");
  let mut parser = Parser::new(&mut input);
  assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_err());
}

#[test]
fn has_disallows_pseudo_elements_in_nested_selector_arguments() {
  let mut input = ParserInput::new("div:has(:host(::before))");
  let mut parser = Parser::new(&mut input);
  assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_err());
}

#[test]
fn has_drops_nested_has_inside_is() {
  let selector = parse_selector("div:has(:is(:has(.foo), .bar))");

  let dom = element_with_class(
    "div",
    None,
    vec![element_with_class("span", Some("bar"), vec![])],
  );
  let element_ref = ElementRef::with_ancestors(&dom, &[]);
  assert!(selector_matches(&element_ref, &selector));

  let dom = element_with_class(
    "div",
    None,
    vec![element_with_class("span", Some("foo"), vec![])],
  );
  let element_ref = ElementRef::with_ancestors(&dom, &[]);
  assert!(!selector_matches(&element_ref, &selector));
}
