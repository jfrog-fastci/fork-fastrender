//! CSS Selector support
//!
//! Implements selector parsing and matching using the selectors crate.

use super::types::CssString;
use crate::dom::AssignedSlot;
use crate::dom::DomNode;
use crate::dom::ElementAttrCache;
use crate::dom::SelectorBloomStore;
use crate::dom::SiblingListCache;
use crate::error::RenderError;
use crate::style::normalize_language_tag;
use cssparser::ParseError;
use cssparser::Parser;
use cssparser::ToCss;
use cssparser::Token;
use rustc_hash::FxHashSet;
use selectors::parser::Selector;
use selectors::parser::SelectorImpl;
use selectors::parser::SelectorList;
use selectors::parser::SelectorParseErrorKind;
use selectors::parser::{
  Combinator, RelativeSelector, RelativeSelectorAncestorHashes, RelativeSelectorBloomHashes,
  RelativeSelectorMatchHint,
};
use selectors::OpaqueElement;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;

/// Direction keyword for :dir()
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDirection {
  Ltr,
  Rtl,
}

// ============================================================================
// Selector implementation for FastRender
// ============================================================================

/// Our custom SelectorImpl for FastRender
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastRenderSelectorImpl;

/// Additional per-match context needed for shadow-aware selector evaluation.
#[derive(Debug)]
pub struct ShadowMatchData<'a> {
  /// The shadow host for the stylesheet being matched, or None for document styles.
  pub shadow_host: Option<OpaqueElement>,
  /// Mapping from slot elements to their assigned nodes for ::slotted() resolution.
  pub slot_map: Option<&'a SlotAssignmentMap<'a>>,
  /// Exported part mappings for resolving ::part() across shadow boundaries.
  pub part_export_map: Option<&'a PartExportMap>,
  /// Deferred error from deadline checks performed during selector matching.
  pub deadline_error: Option<RenderError>,
  /// Precomputed selector bloom summaries for fast :has() pruning.
  pub selector_blooms: Option<&'a SelectorBloomStore>,
  /// Mapping from DOM node pointers to their stable pre-order `node_id` (for bloom-summary lookups).
  pub node_to_id: Option<&'a HashMap<*const DomNode, usize>>,
  /// Cached sibling positions for structural pseudo-classes.
  pub sibling_cache: Option<&'a SiblingListCache>,
  /// Per-pass cache for expensive element attribute lookups during selector matching.
  pub element_attr_cache: Option<&'a ElementAttrCache>,
  /// Per-pass cache for `form:valid/invalid` and `fieldset:valid/invalid` propagation.
  pub form_validity_index: Option<&'a FormValidityIndex>,
  /// When true, treat custom elements as always defined for `:defined` pseudo-class matching.
  ///
  /// The spec behavior is that elements with a valid custom-element name are *not* `:defined`
  /// unless they have been upgraded by the custom elements registry. FastRender does not run the
  /// registry, but always treating custom elements as defined can improve real-world compatibility
  /// (many pages hide content behind `:not(:defined)`).
  ///
  /// Default: `true` (compatibility).
  pub treat_custom_elements_as_defined: bool,
}

impl<'a> Default for ShadowMatchData<'a> {
  fn default() -> Self {
    Self {
      shadow_host: None,
      slot_map: None,
      part_export_map: None,
      deadline_error: None,
      selector_blooms: None,
      node_to_id: None,
      sibling_cache: None,
      element_attr_cache: None,
      form_validity_index: None,
      treat_custom_elements_as_defined: true,
    }
  }
}

impl<'a> ShadowMatchData<'a> {
  pub fn for_document() -> Self {
    Self::default()
  }

  pub fn for_shadow_host(shadow_host: OpaqueElement) -> Self {
    Self {
      shadow_host: Some(shadow_host),
      ..Self::default()
    }
  }

  pub fn with_slot_map(mut self, slot_map: &'a SlotAssignmentMap<'a>) -> Self {
    self.slot_map = Some(slot_map);
    self
  }

  pub fn with_part_export_map(mut self, part_export_map: Option<&'a PartExportMap>) -> Self {
    self.part_export_map = part_export_map;
    self
  }

  pub fn record_deadline_error(&mut self, err: RenderError) {
    if self.deadline_error.is_none() {
      self.deadline_error = Some(err);
    }
  }

  pub fn with_selector_blooms(mut self, selector_blooms: Option<&'a SelectorBloomStore>) -> Self {
    self.selector_blooms = selector_blooms;
    self
  }

  pub fn with_node_to_id(mut self, node_to_id: Option<&'a HashMap<*const DomNode, usize>>) -> Self {
    self.node_to_id = node_to_id;
    self
  }

  pub fn node_id_for(&self, node: &DomNode) -> Option<usize> {
    self
      .node_to_id
      .and_then(|map| map.get(&(node as *const DomNode)).copied())
  }

  pub fn with_sibling_cache(mut self, sibling_cache: &'a SiblingListCache) -> Self {
    self.sibling_cache = Some(sibling_cache);
    self
  }

  pub fn with_element_attr_cache(mut self, element_attr_cache: &'a ElementAttrCache) -> Self {
    self.element_attr_cache = Some(element_attr_cache);
    self
  }

  pub fn with_form_validity_index(mut self, form_validity_index: &'a FormValidityIndex) -> Self {
    self.form_validity_index = Some(form_validity_index);
    self
  }

  pub fn with_custom_elements_defined(mut self, enabled: bool) -> Self {
    self.treat_custom_elements_as_defined = enabled;
    self
  }
}

/// Per-document index for `form:valid/invalid` and `fieldset:valid/invalid` selectors.
///
/// This is built once per cascade pass and queried during selector matching to avoid repeatedly
/// scanning descendant controls for every `<form>`/`<fieldset>` node.
#[derive(Debug, Default)]
pub struct FormValidityIndex {
  invalid_forms: FxHashSet<*const DomNode>,
  invalid_fieldsets: FxHashSet<*const DomNode>,
}

impl FormValidityIndex {
  pub fn insert_invalid_form(&mut self, form: *const DomNode) {
    self.invalid_forms.insert(form);
  }

  pub fn insert_invalid_fieldset(&mut self, fieldset: *const DomNode) {
    self.invalid_fieldsets.insert(fieldset);
  }

  pub fn form_is_invalid(&self, form: &DomNode) -> bool {
    self.invalid_forms.contains(&(form as *const DomNode))
  }

  pub fn fieldset_is_invalid(&self, fieldset: &DomNode) -> bool {
    self
      .invalid_fieldsets
      .contains(&(fieldset as *const DomNode))
  }
}

/// Mapping helpers for shadow slot assignments during selector matching.
#[derive(Debug, Clone)]
pub struct SlotAssignmentMap<'a> {
  pub slot_to_nodes: HashMap<usize, Vec<usize>>,
  pub node_to_slot: HashMap<usize, AssignedSlot>,
  pub slot_ancestors: HashMap<usize, Vec<&'a DomNode>>,
  // Node ids are stable pre-order traversal indices, so store id->node mappings densely.
  pub id_to_node: Vec<*const DomNode>,
  pub node_to_id: HashMap<*const DomNode, usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct AssignedSlotRef<'a> {
  pub slot: &'a DomNode,
  pub ancestors: &'a [&'a DomNode],
  pub shadow_root_id: usize,
}

impl<'a> SlotAssignmentMap<'a> {
  pub fn new(
    node_to_id: &HashMap<*const DomNode, usize>,
    id_to_node: &Vec<*const DomNode>,
  ) -> Self {
    Self {
      slot_to_nodes: HashMap::new(),
      node_to_slot: HashMap::new(),
      slot_ancestors: HashMap::new(),
      id_to_node: id_to_node.clone(),
      node_to_id: node_to_id.clone(),
    }
  }

  pub fn add_slot(
    &mut self,
    slot: &'a DomNode,
    ancestors: Vec<&'a DomNode>,
    assigned_nodes: Vec<&'a DomNode>,
    shadow_root_id: usize,
  ) {
    let Some(slot_id) = self.slot_id(slot) else {
      return;
    };

    let assigned_ids: Vec<usize> = assigned_nodes
      .iter()
      .filter_map(|node| self.node_id(node))
      .collect();
    if assigned_ids.is_empty() {
      return;
    }

    self.slot_ancestors.entry(slot_id).or_insert(ancestors);
    let slot_name = slot.get_attribute_ref("name").unwrap_or("").to_string();
    for node_id in assigned_ids.iter().copied() {
      self.node_to_slot.insert(
        node_id,
        AssignedSlot {
          slot_name: slot_name.clone(),
          slot_node_id: slot_id,
          shadow_root_id,
        },
      );
    }

    self.slot_to_nodes.insert(slot_id, assigned_ids);
  }

  pub fn slot_id(&self, slot: &DomNode) -> Option<usize> {
    self.node_to_id.get(&(slot as *const DomNode)).copied()
  }

  pub fn node_id(&self, node: &DomNode) -> Option<usize> {
    self.node_to_id.get(&(node as *const DomNode)).copied()
  }

  pub fn assigned_node_ids(&self, slot_id: usize) -> Option<&[usize]> {
    self
      .slot_to_nodes
      .get(&slot_id)
      .map(|nodes| nodes.as_slice())
  }

  pub fn node_for_id(&self, node_id: usize) -> Option<&'a DomNode> {
    self
      .id_to_node
      .get(node_id)
      .copied()
      .filter(|ptr| !ptr.is_null())
      .map(|ptr| unsafe { &*ptr })
  }

  pub fn assigned_slot(&'a self, node: &DomNode) -> Option<AssignedSlotRef<'a>> {
    let node_id = self.node_id(node)?;
    let slot = self.node_to_slot.get(&node_id)?;
    let slot_node = self.node_for_id(slot.slot_node_id)?;
    let ancestors = self.slot_ancestors.get(&slot.slot_node_id)?;
    Some(AssignedSlotRef {
      slot: slot_node,
      ancestors,
      shadow_root_id: slot.shadow_root_id,
    })
  }
}

/// A target in a shadow root's part map.
///
/// `::part()` selectors can match both real elements (via `part="..."`) and fully-styleable
/// pseudo-elements that have been forwarded via `exportparts="::before: name"` etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportedPartTarget {
  Element(usize),
  Pseudo { node_id: usize, pseudo: PseudoElement },
}

/// Mapping from shadow hosts to their resolved part element maps.
#[derive(Debug, Default, Clone)]
pub struct PartExportMap {
  hosts: HashMap<usize, HashMap<String, Vec<ExportedPartTarget>>>,
}

impl PartExportMap {
  pub fn exports_for_host(
    &self,
    host: usize,
  ) -> Option<&HashMap<String, Vec<ExportedPartTarget>>> {
    self.hosts.get(&host)
  }

  pub fn insert_host_exports(
    &mut self,
    host: usize,
    exports: HashMap<String, Vec<ExportedPartTarget>>,
  ) {
    self.hosts.insert(host, exports);
  }
}

impl SelectorImpl for FastRenderSelectorImpl {
  type AttrValue = CssString;
  type BorrowedLocalName = str;
  type BorrowedNamespaceUrl = str;
  type ExtraMatchingData<'a> = ShadowMatchData<'a>;
  type Identifier = CssString;
  type LocalName = CssString;
  type NamespacePrefix = CssString;
  type NamespaceUrl = CssString;
  type NonTSPseudoClass = PseudoClass;
  type PseudoElement = PseudoElement;

  fn should_collect_attr_hash(_name: &Self::LocalName) -> bool {
    // Attribute selectors are indexed in RuleIndex; allow bloom filters to prune on them.
    true
  }
}

// ============================================================================
// Pseudo-classes
// ============================================================================

/// Pseudo-classes we support
#[derive(Clone, PartialEq, Eq)]
pub enum PseudoClass {
  Has(Box<[RelativeSelector<FastRenderSelectorImpl>]>),
  Host(Option<SelectorList<FastRenderSelectorImpl>>),
  HostContext(SelectorList<FastRenderSelectorImpl>),
  Root,
  Defined,
  FirstChild,
  LastChild,
  NthChild(i32, i32, Option<SelectorList<FastRenderSelectorImpl>>), // an + b
  NthLastChild(i32, i32, Option<SelectorList<FastRenderSelectorImpl>>),
  OnlyChild,
  FirstOfType,
  LastOfType,
  OnlyOfType,
  NthOfType(i32, i32),
  NthLastOfType(i32, i32),
  Lang(Vec<String>),
  Dir(TextDirection),
  AnyLink,
  Target,
  TargetWithin,
  Scope,
  Empty,
  Hover,
  Active,
  Focus,
  FocusWithin,
  FocusVisible,
  Fullscreen,
  Open,
  Modal,
  PopoverOpen,
  Disabled,
  Enabled,
  Required,
  Optional,
  Valid,
  Invalid,
  UserValid,
  UserInvalid,
  InRange,
  OutOfRange,
  ReadOnly,
  ReadWrite,
  PlaceholderShown,
  WebkitInputPlaceholder,
  MsInputPlaceholder,
  MozPlaceholder,
  Autofill,
  MozUiInvalid,
  MozFocusring,
  Checked,
  Indeterminate,
  Default,
  Link,
  Visited,
  /// Any vendor-specific pseudo-class we don't model, kept to avoid selector-list invalidation.
  Vendor(CssString),
}

impl fmt::Debug for PseudoClass {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      PseudoClass::Has(relative) => {
        let selectors: Vec<String> = relative
          .iter()
          .map(|sel| sel.selector.to_css_string())
          .collect();
        f.debug_tuple("Has").field(&selectors).finish()
      }
      _ => f.write_str(&self.to_css_string()),
    }
  }
}

impl selectors::parser::NonTSPseudoClass for PseudoClass {
  type Impl = FastRenderSelectorImpl;

  fn is_active_or_hover(&self) -> bool {
    matches!(self, PseudoClass::Active | PseudoClass::Hover)
  }

  fn is_user_action_state(&self) -> bool {
    matches!(
      self,
      PseudoClass::Hover
        | PseudoClass::Active
        | PseudoClass::Focus
        | PseudoClass::FocusWithin
        | PseudoClass::FocusVisible
    )
  }

  fn specificity(&self) -> u32 {
    const PSEUDO_CLASS_SPECIFICITY: u32 = 1 << 10;
    const MAX_10BIT: u32 = (1u32 << 10) - 1;
    // Specificity values use a packed 10-bit per-component encoding:
    //   A (IDs)    = bits 20..29
    //   B (class)  = bits 10..19
    //   C (type)   = bits  0..9
    //
    // When adding specificity from nested selector arguments, we must clamp each component to
    // avoid overflowing into higher components (e.g. B overflowing into A). This mirrors the
    // selectors crate's internal `Specificity` accumulation behavior.
    let add_specificity = |base: u32, extra: u32| -> u32 {
      let base_a = base >> 20;
      let base_b = (base >> 10) & MAX_10BIT;
      let base_c = base & MAX_10BIT;

      let extra_a = extra >> 20;
      let extra_b = (extra >> 10) & MAX_10BIT;
      let extra_c = extra & MAX_10BIT;

      let a = base_a.saturating_add(extra_a).min(MAX_10BIT);
      let b = base_b.saturating_add(extra_b).min(MAX_10BIT);
      let c = base_c.saturating_add(extra_c).min(MAX_10BIT);

      (a << 20) | (b << 10) | c
    };
    let argument_specificity = |selectors: &SelectorList<FastRenderSelectorImpl>| {
      selectors
        .slice()
        .iter()
        .map(|selector| selector.specificity())
        .max()
        .unwrap_or(0)
    };
    match self {
      PseudoClass::Has(relative) => relative
        .iter()
        .map(|selector| selector.selector.specificity())
        .max()
        .unwrap_or(0),
      // Per Selectors Level 4 §17 (Calculating a selector's specificity), these pseudo-classes add
      // the specificity of the most specific selector in their `of <selector-list>` argument.
      PseudoClass::NthChild(_, _, Some(selectors))
      | PseudoClass::NthLastChild(_, _, Some(selectors)) => {
        add_specificity(PSEUDO_CLASS_SPECIFICITY, argument_specificity(selectors))
      }
      PseudoClass::Host(None) => PSEUDO_CLASS_SPECIFICITY,
      PseudoClass::Host(Some(selectors)) => {
        add_specificity(PSEUDO_CLASS_SPECIFICITY, argument_specificity(selectors))
      }
      PseudoClass::HostContext(selectors) => {
        add_specificity(PSEUDO_CLASS_SPECIFICITY, argument_specificity(selectors))
      }
      _ => PSEUDO_CLASS_SPECIFICITY, // Pseudo-classes have class-level specificity by default.
    }
  }

  fn is_has(&self) -> bool {
    matches!(self, PseudoClass::Has(..))
  }

  fn matches_featureless_host(&self) -> selectors::parser::MatchesFeaturelessHost {
    match self {
      PseudoClass::Host(_) | PseudoClass::HostContext(_) => {
        selectors::parser::MatchesFeaturelessHost::Only
      }
      _ => selectors::parser::MatchesFeaturelessHost::Never,
    }
  }
}

fn write_nth_pseudo<W: fmt::Write>(
  dest: &mut W,
  name: &str,
  a: i32,
  b: i32,
  of: &Option<SelectorList<FastRenderSelectorImpl>>,
) -> fmt::Result {
  write!(dest, "{}({}n+{}", name, a, b)?;
  if let Some(selectors) = of {
    dest.write_str(" of ")?;
    selectors.to_css(dest)?;
  }
  dest.write_str(")")
}

impl ToCss for PseudoClass {
  fn to_css<W>(&self, dest: &mut W) -> fmt::Result
  where
    W: fmt::Write,
  {
    match self {
      PseudoClass::Has(selectors) => {
        dest.write_str(":has(")?;
        let mut first = true;
        for rel in selectors.iter() {
          if !first {
            dest.write_str(", ")?;
          }
          first = false;
          rel.selector.to_css(dest)?;
        }
        dest.write_str(")")
      }
      PseudoClass::Host(None) => dest.write_str(":host"),
      PseudoClass::Host(Some(selectors)) => {
        dest.write_str(":host(")?;
        selectors.to_css(dest)?;
        dest.write_str(")")
      }
      PseudoClass::HostContext(selectors) => {
        dest.write_str(":host-context(")?;
        selectors.to_css(dest)?;
        dest.write_str(")")
      }
      PseudoClass::Root => dest.write_str(":root"),
      PseudoClass::Defined => dest.write_str(":defined"),
      PseudoClass::FirstChild => dest.write_str(":first-child"),
      PseudoClass::LastChild => dest.write_str(":last-child"),
      PseudoClass::NthChild(a, b, of) => write_nth_pseudo(dest, ":nth-child", *a, *b, of),
      PseudoClass::NthLastChild(a, b, of) => write_nth_pseudo(dest, ":nth-last-child", *a, *b, of),
      PseudoClass::OnlyChild => dest.write_str(":only-child"),
      PseudoClass::FirstOfType => dest.write_str(":first-of-type"),
      PseudoClass::LastOfType => dest.write_str(":last-of-type"),
      PseudoClass::OnlyOfType => dest.write_str(":only-of-type"),
      PseudoClass::NthOfType(a, b) => write!(dest, ":nth-of-type({}n+{})", a, b),
      PseudoClass::NthLastOfType(a, b) => write!(dest, ":nth-last-of-type({}n+{})", a, b),
      PseudoClass::Lang(langs) => {
        dest.write_str(":lang(")?;
        for (i, lang) in langs.iter().enumerate() {
          if i > 0 {
            dest.write_str(", ")?;
          }
          dest.write_str(lang)?;
        }
        dest.write_str(")")
      }
      PseudoClass::Dir(dir) => match dir {
        TextDirection::Ltr => dest.write_str(":dir(ltr)"),
        TextDirection::Rtl => dest.write_str(":dir(rtl)"),
      },
      PseudoClass::AnyLink => dest.write_str(":any-link"),
      PseudoClass::Target => dest.write_str(":target"),
      PseudoClass::TargetWithin => dest.write_str(":target-within"),
      PseudoClass::Scope => dest.write_str(":scope"),
      PseudoClass::Empty => dest.write_str(":empty"),
      PseudoClass::Hover => dest.write_str(":hover"),
      PseudoClass::Active => dest.write_str(":active"),
      PseudoClass::Focus => dest.write_str(":focus"),
      PseudoClass::FocusWithin => dest.write_str(":focus-within"),
      PseudoClass::FocusVisible => dest.write_str(":focus-visible"),
      PseudoClass::Fullscreen => dest.write_str(":fullscreen"),
      PseudoClass::Open => dest.write_str(":open"),
      PseudoClass::Modal => dest.write_str(":modal"),
      PseudoClass::PopoverOpen => dest.write_str(":popover-open"),
      PseudoClass::Disabled => dest.write_str(":disabled"),
      PseudoClass::Enabled => dest.write_str(":enabled"),
      PseudoClass::Required => dest.write_str(":required"),
      PseudoClass::Optional => dest.write_str(":optional"),
      PseudoClass::Valid => dest.write_str(":valid"),
      PseudoClass::Invalid => dest.write_str(":invalid"),
      PseudoClass::UserValid => dest.write_str(":user-valid"),
      PseudoClass::UserInvalid => dest.write_str(":user-invalid"),
      PseudoClass::InRange => dest.write_str(":in-range"),
      PseudoClass::OutOfRange => dest.write_str(":out-of-range"),
      PseudoClass::Indeterminate => dest.write_str(":indeterminate"),
      PseudoClass::Default => dest.write_str(":default"),
      PseudoClass::ReadOnly => dest.write_str(":read-only"),
      PseudoClass::ReadWrite => dest.write_str(":read-write"),
      PseudoClass::PlaceholderShown => dest.write_str(":placeholder-shown"),
      PseudoClass::WebkitInputPlaceholder => dest.write_str(":-webkit-input-placeholder"),
      PseudoClass::MsInputPlaceholder => dest.write_str(":-ms-input-placeholder"),
      PseudoClass::MozPlaceholder => dest.write_str(":-moz-placeholder"),
      PseudoClass::Autofill => dest.write_str(":autofill"),
      PseudoClass::MozUiInvalid => dest.write_str(":-moz-ui-invalid"),
      PseudoClass::MozFocusring => dest.write_str(":-moz-focusring"),
      PseudoClass::Checked => dest.write_str(":checked"),
      PseudoClass::Link => dest.write_str(":link"),
      PseudoClass::Visited => dest.write_str(":visited"),
      PseudoClass::Vendor(name) => {
        dest.write_str(":")?;
        name.to_css(dest)
      }
    }
  }
}

// ============================================================================
// Pseudo-elements
// ============================================================================

/// Pseudo-elements we support
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoElement {
  Before,
  After,
  FirstLine,
  FirstLetter,
  Marker,
  FootnoteCall,
  FootnoteMarker,
  Backdrop,
  /// Placeholder text for form controls (input/textarea).
  Placeholder,
  /// File upload button pseudo-element for `<input type="file">` (vendor aliases mapped here).
  FileSelectorButton,
  Selection,
  MozFocusInner,
  MozFocusOuter,
  /// Range slider thumb pseudo-element (vendor aliases mapped here).
  SliderThumb,
  /// Range slider track pseudo-element (vendor aliases mapped here).
  SliderTrack,
  /// Any vendor-specific pseudo-element we don't model, kept to avoid selector-list invalidation.
  Vendor(CssString),
  Slotted(Box<[Selector<FastRenderSelectorImpl>]>),
  /// Per CSS Shadow Parts, `::part()` accepts one or more idents; when multiple are supplied the
  /// pseudo represents the intersection of those part-name buckets.
  Part(Box<[CssString]>),
}

impl selectors::parser::PseudoElement for PseudoElement {
  type Impl = FastRenderSelectorImpl;
}

impl std::hash::Hash for PseudoElement {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    std::mem::discriminant(self).hash(state);
    match self {
      PseudoElement::Slotted(selectors) => {
        struct HashWriter<'a, H: std::hash::Hasher>(&'a mut H);

        impl<H: std::hash::Hasher> fmt::Write for HashWriter<'_, H> {
          fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0.write(s.as_bytes());
            Ok(())
          }
        }

        selectors.len().hash(state);
        let mut writer = HashWriter(state);
        for selector in selectors.iter() {
          writer.0.write_u8(0);
          let _ = selector.to_css(&mut writer);
        }
      }
      PseudoElement::Part(names) => {
        names.len().hash(state);
        for name in names.iter() {
          name.hash(state);
        }
      }
      PseudoElement::Vendor(name) => name.hash(state),
      _ => {}
    }
  }
}

impl PseudoElement {
  /// Pseudo-elements that generate their own boxes (::before/::after/::marker/::backdrop).
  pub fn is_generated_box(&self) -> bool {
    matches!(
      self,
      PseudoElement::Before
        | PseudoElement::After
        | PseudoElement::Marker
        | PseudoElement::Backdrop
    )
  }
}

impl ToCss for PseudoElement {
  fn to_css<W>(&self, dest: &mut W) -> fmt::Result
  where
    W: fmt::Write,
  {
    match self {
      PseudoElement::Before => dest.write_str("::before"),
      PseudoElement::After => dest.write_str("::after"),
      PseudoElement::FirstLine => dest.write_str("::first-line"),
      PseudoElement::FirstLetter => dest.write_str("::first-letter"),
      PseudoElement::Marker => dest.write_str("::marker"),
      PseudoElement::FootnoteCall => dest.write_str("::footnote-call"),
      PseudoElement::FootnoteMarker => dest.write_str("::footnote-marker"),
      PseudoElement::Backdrop => dest.write_str("::backdrop"),
      PseudoElement::Placeholder => dest.write_str("::placeholder"),
      PseudoElement::FileSelectorButton => dest.write_str("::file-selector-button"),
      PseudoElement::Selection => dest.write_str("::selection"),
      PseudoElement::MozFocusInner => dest.write_str("::-moz-focus-inner"),
      PseudoElement::MozFocusOuter => dest.write_str("::-moz-focus-outer"),
      // There is no standards-track name for these pseudo-elements today; serialize to a canonical
      // vendor spelling so debugging and tests remain stable.
      PseudoElement::SliderThumb => dest.write_str("::-webkit-slider-thumb"),
      PseudoElement::SliderTrack => dest.write_str("::-webkit-slider-runnable-track"),
      PseudoElement::Vendor(name) => {
        dest.write_str("::")?;
        name.to_css(dest)
      }
      PseudoElement::Slotted(selectors) => {
        dest.write_str("::slotted(")?;
        for (i, selector) in selectors.iter().enumerate() {
          if i > 0 {
            dest.write_str(", ")?;
          }
          selector.to_css(dest)?;
        }
        dest.write_str(")")
      }
      PseudoElement::Part(names) => {
        dest.write_str("::part(")?;
        for (i, name) in names.iter().enumerate() {
          if i > 0 {
            dest.write_str(" ")?;
          }
          name.to_css(dest)?;
        }
        dest.write_str(")")
      }
    }
  }
}

// ============================================================================
// Pseudo-class parser
// ============================================================================

/// Namespace declarations parsed from `@namespace` rules.
///
/// This is stored in thread-local storage so selector parsing (which happens in many places,
/// including stylesheet parsing and DOM APIs) can access the current stylesheet's namespace
/// mappings without threading state through every selectors-crate callback.
#[derive(Debug, Clone, Default)]
pub(crate) struct NamespaceContext {
  pub default: Option<CssString>,
  pub prefixes: HashMap<CssString, CssString>,
}

thread_local! {
  static NAMESPACE_CONTEXT: RefCell<NamespaceContext> = RefCell::new(NamespaceContext::default());
}

/// RAII guard that sets up a fresh namespace context for a stylesheet parse and restores the
/// previous context when dropped.
pub(crate) struct NamespaceContextGuard {
  previous: NamespaceContext,
}

impl NamespaceContextGuard {
  pub(crate) fn new() -> Self {
    let previous = NAMESPACE_CONTEXT.with(|ctx| ctx.replace(NamespaceContext::default()));
    Self { previous }
  }
}

impl Drop for NamespaceContextGuard {
  fn drop(&mut self) {
    let previous = self.previous.clone();
    NAMESPACE_CONTEXT.with(|ctx| *ctx.borrow_mut() = previous);
  }
}

pub(crate) fn namespace_context_set_default(url: CssString) {
  NAMESPACE_CONTEXT.with(|ctx| ctx.borrow_mut().default = Some(url));
}

pub(crate) fn namespace_context_set_prefix(prefix: &str, url: CssString) {
  let key = CssString::from(prefix.to_ascii_lowercase());
  NAMESPACE_CONTEXT.with(|ctx| {
    ctx.borrow_mut().prefixes.insert(key, url);
  });
}

/// Custom parser for pseudo-classes
/// Public parser entrypoint for selector parsing.
///
/// This is exposed so fuzzers and tools outside the crate can reuse the
/// canonical parser configuration without duplicating selector setup.
pub struct PseudoClassParser;

// Selectors Level 4:
// - Pseudo-elements are not valid selectors within `:has()`, unless explicitly defined as
//   `:has-allowed pseudo-elements` (none are defined by Selectors 4).
//
// We implement this as a parse-time restriction by tracking when we're parsing inside a `:has()`
// argument and rejecting pseudo-elements at the point they're encountered. This ensures invalid
// sub-selectors can still be dropped by forgiving list parsers like `:is()` / `:where()`.
thread_local! {
  static IN_HAS_ARGUMENT: Cell<u32> = const { Cell::new(0) };
}

fn parsing_has_argument() -> bool {
  IN_HAS_ARGUMENT.with(|depth| depth.get() > 0)
}

struct HasArgumentScope;

impl HasArgumentScope {
  fn enter() -> Self {
    IN_HAS_ARGUMENT.with(|depth| depth.set(depth.get() + 1));
    Self
  }
}

impl Drop for HasArgumentScope {
  fn drop(&mut self) {
    IN_HAS_ARGUMENT.with(|depth| depth.set(depth.get().saturating_sub(1)));
  }
}

impl<'i> selectors::parser::Parser<'i> for PseudoClassParser {
  type Error = SelectorParseErrorKind<'i>;
  type Impl = FastRenderSelectorImpl;

  fn default_namespace(&self) -> Option<<Self::Impl as SelectorImpl>::NamespaceUrl> {
    NAMESPACE_CONTEXT.with(|ctx| ctx.borrow().default.clone())
  }

  fn namespace_for_prefix(
    &self,
    prefix: &<Self::Impl as SelectorImpl>::NamespacePrefix,
  ) -> Option<<Self::Impl as SelectorImpl>::NamespaceUrl> {
    let prefix = prefix.as_str();
    NAMESPACE_CONTEXT.with(|ctx| {
      let ctx = ctx.borrow();
      if prefix.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower = prefix.to_ascii_lowercase();
        ctx.prefixes.get(lower.as_str()).cloned()
      } else {
        ctx.prefixes.get(prefix).cloned()
      }
    })
  }

  fn parse_nth_child_of(&self) -> bool {
    true
  }

  fn is_pseudo_element_with_single_colon(&self, name: &str) -> bool {
    match name.to_ascii_lowercase().as_str() {
      // Real-world stylesheets (and some engines) still use single-colon forms for these
      // vendor pseudo-elements.
      "selection" | "-moz-selection" => true,
      // Non-vendor pseudo-elements that are frequently written with a single colon in
      // older stylesheets.
      "marker" | "backdrop" => true,
      // `::marker`-like vendor pseudo-elements.
      "-moz-list-bullet" | "-moz-list-number" | "-webkit-details-marker" => true,
      // `::backdrop` vendor aliases are commonly used in single-colon form.
      "-webkit-backdrop" | "-ms-backdrop" => true,
      // Functional pseudo-elements.
      "part" | "slotted" => true,
      "placeholder"
      | "-webkit-input-placeholder"
      | "-moz-placeholder"
      | "-ms-input-placeholder" => true,
      // File input upload button.
      "file-selector-button" | "-webkit-file-upload-button" => true,
      "-moz-focus-inner" => true,
      "-moz-focus-outer" => true,
      // Standard slider pseudo-elements (CSS Forms). Treat these like their vendor equivalents so
      // authored selectors like `input::slider-thumb` match.
      "slider-thumb" | "slider-track" => true,
      "-webkit-slider-thumb" | "-moz-range-thumb" | "-ms-thumb" => true,
      "-webkit-slider-runnable-track" | "-moz-range-track" | "-ms-track" => true,
      _ => false,
    }
  }

  fn parse_non_ts_pseudo_class(
    &self,
    _location: cssparser::SourceLocation,
    name: cssparser::CowRcStr<'i>,
  ) -> std::result::Result<PseudoClass, ParseError<'i, Self::Error>> {
    let lowered = name.to_ascii_lowercase();
    match lowered.as_str() {
      "host" => Ok(PseudoClass::Host(None)),
      "root" => Ok(PseudoClass::Root),
      "defined" => Ok(PseudoClass::Defined),
      "first-child" => Ok(PseudoClass::FirstChild),
      "last-child" => Ok(PseudoClass::LastChild),
      "only-child" => Ok(PseudoClass::OnlyChild),
      "first-of-type" => Ok(PseudoClass::FirstOfType),
      "last-of-type" => Ok(PseudoClass::LastOfType),
      "only-of-type" => Ok(PseudoClass::OnlyOfType),
      "empty" => Ok(PseudoClass::Empty),
      "hover" => Ok(PseudoClass::Hover),
      "active" => Ok(PseudoClass::Active),
      "focus" => Ok(PseudoClass::Focus),
      "focus-within" => Ok(PseudoClass::FocusWithin),
      "focus-visible" => Ok(PseudoClass::FocusVisible),
      "fullscreen" => Ok(PseudoClass::Fullscreen),
      "open" => Ok(PseudoClass::Open),
      "modal" => Ok(PseudoClass::Modal),
      "popover-open" => Ok(PseudoClass::PopoverOpen),
      "-webkit-full-screen" => Ok(PseudoClass::Fullscreen),
      "-moz-full-screen" => Ok(PseudoClass::Fullscreen),
      "-ms-fullscreen" => Ok(PseudoClass::Fullscreen),
      "disabled" => Ok(PseudoClass::Disabled),
      "enabled" => Ok(PseudoClass::Enabled),
      "required" => Ok(PseudoClass::Required),
      "optional" => Ok(PseudoClass::Optional),
      "valid" => Ok(PseudoClass::Valid),
      "invalid" => Ok(PseudoClass::Invalid),
      "user-valid" => Ok(PseudoClass::UserValid),
      "user-invalid" => Ok(PseudoClass::UserInvalid),
      "in-range" => Ok(PseudoClass::InRange),
      "out-of-range" => Ok(PseudoClass::OutOfRange),
      "indeterminate" => Ok(PseudoClass::Indeterminate),
      "default" => Ok(PseudoClass::Default),
      "read-only" => Ok(PseudoClass::ReadOnly),
      "read-write" => Ok(PseudoClass::ReadWrite),
      "-moz-read-only" => Ok(PseudoClass::ReadOnly),
      "-moz-read-write" => Ok(PseudoClass::ReadWrite),
      "placeholder-shown" => Ok(PseudoClass::PlaceholderShown),
      "-webkit-input-placeholder" => Ok(PseudoClass::WebkitInputPlaceholder),
      "-ms-input-placeholder" => Ok(PseudoClass::MsInputPlaceholder),
      "-moz-placeholder" => Ok(PseudoClass::MozPlaceholder),
      "-moz-placeholder-shown" => Ok(PseudoClass::PlaceholderShown),
      "autofill" => Ok(PseudoClass::Autofill),
      "-webkit-autofill" => Ok(PseudoClass::Autofill),
      "-moz-autofill" => Ok(PseudoClass::Autofill),
      "-moz-ui-invalid" => Ok(PseudoClass::MozUiInvalid),
      "-moz-focusring" => Ok(PseudoClass::MozFocusring),
      "checked" => Ok(PseudoClass::Checked),
      "link" => Ok(PseudoClass::Link),
      "visited" => Ok(PseudoClass::Visited),
      "any-link" => Ok(PseudoClass::AnyLink),
      "-webkit-any-link" => Ok(PseudoClass::AnyLink),
      "-moz-any-link" => Ok(PseudoClass::AnyLink),
      "target" => Ok(PseudoClass::Target),
      "target-within" => Ok(PseudoClass::TargetWithin),
      "scope" => Ok(PseudoClass::Scope),
      s if s.starts_with("-moz-")
        || s.starts_with("-webkit-")
        || s.starts_with("-ms-")
        || s.starts_with("-o-")
        || s.starts_with("-khtml-") =>
      {
        Ok(PseudoClass::Vendor(CssString::from(s)))
      }
      _ => Err(ParseError {
        kind: cssparser::ParseErrorKind::Custom(
          SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name),
        ),
        location: _location,
      }),
    }
  }

  fn parse_non_ts_functional_pseudo_class<'t>(
    &self,
    name: cssparser::CowRcStr<'i>,
    parser: &mut Parser<'i, 't>,
    _is_starting_single_colon: bool,
  ) -> std::result::Result<PseudoClass, ParseError<'i, Self::Error>> {
    let lowered = name.to_ascii_lowercase();
    match lowered.as_str() {
      "host" => {
        let selectors = SelectorList::parse(
          &PseudoClassParser,
          parser,
          selectors::parser::ParseRelative::No,
        )
        .map_err(|_| {
          parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
            name.clone(),
          ))
        })?;
        if selectors.slice().iter().any(selector_has_combinators) {
          return Err(parser.new_custom_error(
            SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name.clone()),
          ));
        }
        Ok(PseudoClass::Host(Some(selectors)))
      }
      "host-context" => {
        let selectors = SelectorList::parse_disallow_pseudo(
          &PseudoClassParser,
          parser,
          selectors::parser::ParseRelative::No,
        )
        .map_err(|_| {
          parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
            name.clone(),
          ))
        })?;
        if selectors.slice().len() != 1 || selectors.slice().iter().any(selector_has_combinators) {
          return Err(parser.new_custom_error(
            SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name.clone()),
          ));
        }
        Ok(PseudoClass::HostContext(selectors))
      }
      "has" => {
        if parsing_has_argument() {
          // Selectors Level 4 forbids nested `:has()`. Treat this as an "invalid state" error so
          // forgiving selector list parsers (e.g. inside `:is()` / `:where()`) can drop only the
          // offending selector without failing the entire selector list.
          return Err(parser.new_custom_error(SelectorParseErrorKind::InvalidState));
        }
        let _scope = HasArgumentScope::enter();
        let list = SelectorList::parse(
          &PseudoClassParser,
          parser,
          selectors::parser::ParseRelative::ForHas,
        )
        .map_err(|_| {
          parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
            name.clone(),
          ))
        })?;
        let relative = build_relative_selectors(list);
        Ok(PseudoClass::Has(relative))
      }
      "nth-child" => {
        let (a, b, of) = parse_nth_with_of(&name, parser)?;
        Ok(PseudoClass::NthChild(a, b, of))
      }
      "nth-last-child" => {
        let (a, b, of) = parse_nth_with_of(&name, parser)?;
        Ok(PseudoClass::NthLastChild(a, b, of))
      }
      "nth-of-type" => {
        let (a, b) = parse_nth(parser).map_err(|_| {
          parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
            name.clone(),
          ))
        })?;
        Ok(PseudoClass::NthOfType(a, b))
      }
      "nth-last-of-type" => {
        let (a, b) = parse_nth(parser).map_err(|_| {
          parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
            name.clone(),
          ))
        })?;
        Ok(PseudoClass::NthLastOfType(a, b))
      }
      "dir" => {
        let dir = match parser.expect_ident() {
          Ok(d) => d,
          Err(_) => {
            return Err(parser.new_custom_error(
              SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name.clone()),
            ))
          }
        };
        let lowered = dir.to_ascii_lowercase();
        match lowered.as_str() {
          "ltr" => Ok(PseudoClass::Dir(TextDirection::Ltr)),
          "rtl" => Ok(PseudoClass::Dir(TextDirection::Rtl)),
          _ => Err(parser.new_custom_error(
            SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name.clone()),
          )),
        }
      }
      "lang" => {
        let mut langs = Vec::new();
        loop {
          let range = match parser.expect_ident_or_string() {
            Ok(r) => r,
            Err(_) => {
              return Err(parser.new_custom_error(
                SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name.clone()),
              ))
            }
          };
          let normalized = normalize_language_tag(range.as_ref());
          if normalized.is_empty() {
            return Err(parser.new_custom_error(
              SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name.clone()),
            ));
          }
          langs.push(normalized);
          if parser.try_parse(|p| p.expect_comma()).is_err() {
            break;
          }
        }
        Ok(PseudoClass::Lang(langs))
      }
      _ => Err(
        parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
          name,
        )),
      ),
    }
  }

  fn parse_pseudo_element(
    &self,
    _location: cssparser::SourceLocation,
    name: cssparser::CowRcStr<'i>,
  ) -> std::result::Result<PseudoElement, ParseError<'i, Self::Error>> {
    if parsing_has_argument() {
      return Err(ParseError {
        kind: cssparser::ParseErrorKind::Custom(SelectorParseErrorKind::InvalidState),
        location: _location,
      });
    }
    let lowered = name.to_ascii_lowercase();
    match lowered.as_str() {
      "before" => Ok(PseudoElement::Before),
      "after" => Ok(PseudoElement::After),
      "first-line" => Ok(PseudoElement::FirstLine),
      "first-letter" => Ok(PseudoElement::FirstLetter),
      "marker" | "-moz-list-bullet" | "-moz-list-number" | "-webkit-details-marker" => {
        Ok(PseudoElement::Marker)
      }
      "footnote-call" => Ok(PseudoElement::FootnoteCall),
      "footnote-marker" => Ok(PseudoElement::FootnoteMarker),
      "backdrop" | "-webkit-backdrop" | "-ms-backdrop" => Ok(PseudoElement::Backdrop),
      "selection" | "-moz-selection" => Ok(PseudoElement::Selection),
      // `::placeholder` has widely used vendor aliases; accept them and canonicalize to the
      // standard name so selector lists containing vendor variants do not invalidate the rule.
      "placeholder"
      | "-webkit-input-placeholder"
      | "-moz-placeholder"
      | "-ms-input-placeholder" => Ok(PseudoElement::Placeholder),
      // `::file-selector-button` is the standards-track spelling; accept the WebKit vendor alias
      // and canonicalize to the standard name so selector lists containing vendor variants do not
      // invalidate the rule.
      "file-selector-button" | "-webkit-file-upload-button" => {
        Ok(PseudoElement::FileSelectorButton)
      }
      "-moz-focus-inner" => Ok(PseudoElement::MozFocusInner),
      "-moz-focus-outer" => Ok(PseudoElement::MozFocusOuter),
      "slider-thumb" | "-webkit-slider-thumb" | "-moz-range-thumb" | "-ms-thumb" => {
        Ok(PseudoElement::SliderThumb)
      }
      "slider-track" | "-webkit-slider-runnable-track" | "-moz-range-track" | "-ms-track" => {
        Ok(PseudoElement::SliderTrack)
      }
      s if s.starts_with("-webkit-")
        || s.starts_with("-moz-")
        || s.starts_with("-ms-")
        || s.starts_with("-o-")
        || s.starts_with("-khtml-") =>
      {
        Ok(PseudoElement::Vendor(CssString::from(s)))
      }
      _ => Err(ParseError {
        kind: cssparser::ParseErrorKind::Custom(
          SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name),
        ),
        location: _location,
      }),
    }
  }

  fn parse_functional_pseudo_element<'t>(
    &self,
    name: cssparser::CowRcStr<'i>,
    parser: &mut Parser<'i, 't>,
  ) -> std::result::Result<PseudoElement, ParseError<'i, Self::Error>> {
    if parsing_has_argument() {
      return Err(parser.new_custom_error(SelectorParseErrorKind::InvalidState));
    }
    let lowered = name.to_ascii_lowercase();
    match lowered.as_str() {
      "slotted" => parse_slotted_pseudo_element(parser, &name),
      "part" => parse_part_pseudo_element(parser, &name),
      _ => Err(
        parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
          name,
        )),
      ),
    }
  }

  fn parse_is_and_where(&self) -> bool {
    // Enable parsing of :is() and :where() pseudo-classes
    true
  }

  fn is_is_alias(&self, name: &str) -> bool {
    // Selectors Level 4 notes that previous drafts used `:matches()` for `:is()`, and UAs may
    // support it as a legacy alias for backwards-compatibility.
    name.eq_ignore_ascii_case("matches")
      // Real-world stylesheets still contain vendor-prefixed `:any()` equivalents.
      || name.eq_ignore_ascii_case("-webkit-any")
      || name.eq_ignore_ascii_case("-moz-any")
  }
}

pub(crate) fn build_relative_selectors(
  selector_list: SelectorList<FastRenderSelectorImpl>,
) -> Box<[RelativeSelector<FastRenderSelectorImpl>]> {
  use selectors::parser::Component;

  fn normalize_leading_scope_relative_selector(
    selector: &Selector<FastRenderSelectorImpl>,
  ) -> Selector<FastRenderSelectorImpl> {
    // The selectors crate always injects a relative selector anchor and a leading combinator when
    // parsing :has() arguments (defaulting to the descendant combinator when omitted).
    //
    // This interacts poorly with authors writing selectors that start with `:scope`, e.g.
    // `:has(:scope > .child)` or `:has(:scope ~ .sibling)`. In that case, the injected descendant
    // combinator becomes a no-op hop to `:scope`, which can never match because `:scope` refers to
    // the anchor element itself (and an element is not a descendant of itself).
    //
    // Normalize such selectors by treating the leading `:scope` compound selector as the anchor,
    // and re-anchoring the selector with the combinator that follows it.
    let components: Vec<Component<FastRenderSelectorImpl>> =
      selector.iter_raw_parse_order_from(0).cloned().collect();
    if components.len() < 4 {
      return selector.clone();
    }

    // Expected parse order for `:has(:scope > .foo)`:
    //   RelativeSelectorAnchor, Descendant, Scope, Child, Class(foo)
    if !matches!(components[0], Component::RelativeSelectorAnchor) {
      return selector.clone();
    }
    if !matches!(components[1], Component::Combinator(Combinator::Descendant)) {
      return selector.clone();
    }
    if !matches!(components[2], Component::Scope) {
      return selector.clone();
    }

    // Find the end of the `:scope...` compound selector.
    let mut scope_compound_end = 3;
    while scope_compound_end < components.len()
      && !matches!(components[scope_compound_end], Component::Combinator(_))
    {
      scope_compound_end += 1;
    }

    // If `:scope` is the last compound, we cannot re-anchor. Fall back to the original selector.
    let Some(Component::Combinator(reanchor_combinator)) = components.get(scope_compound_end)
    else {
      return selector.clone();
    };

    // Preserve the specificity contribution of the removed `:scope` compound selector by adding an
    // always-true `:is(<scope-compound>, *)` to the next compound selector.
    let scope_arg_selector = Selector::from_components(components[2..scope_compound_end].to_vec());
    let universal_selector = Selector::from_components(vec![Component::ExplicitUniversalType]);
    let is_list = SelectorList::from_iter(vec![scope_arg_selector, universal_selector].into_iter());
    let is_component = Component::Is(is_list);

    // Everything after the combinator that follows the scope compound.
    let mut rest = components[(scope_compound_end + 1)..].to_vec();
    // Insert into the first compound of the rest (before the next combinator, if any).
    let mut insert_at = rest.len();
    for (idx, component) in rest.iter().enumerate() {
      if matches!(component, Component::Combinator(_)) {
        insert_at = idx;
        break;
      }
    }
    rest.insert(insert_at, is_component);

    let mut normalized = Vec::with_capacity(2 + rest.len());
    normalized.push(Component::RelativeSelectorAnchor);
    normalized.push(Component::Combinator(*reanchor_combinator));
    normalized.extend(rest);
    Selector::from_components(normalized)
  }

  selector_list
    .slice()
    .iter()
    .map(|selector| {
      let selector = normalize_leading_scope_relative_selector(selector);
      let mut has_child_or_descendants = false;
      let mut has_adjacent_or_next_siblings = false;
      let mut iter = selector.iter_skip_relative_selector_anchor();

      loop {
        while iter.next().is_some() {}
        match iter.next_sequence() {
          Some(Combinator::Descendant) | Some(Combinator::Child) => {
            has_child_or_descendants = true;
          }
          Some(Combinator::NextSibling) | Some(Combinator::LaterSibling) => {
            has_adjacent_or_next_siblings = true;
          }
          Some(_) => {}
          None => break,
        }
      }

      let match_hint = RelativeSelectorMatchHint::new(
        selector.combinator_at_parse_order(1),
        has_child_or_descendants,
        has_adjacent_or_next_siblings,
      );
      let bloom_hashes = RelativeSelectorBloomHashes::new(&selector);
      let ancestor_hashes = RelativeSelectorAncestorHashes::new(&selector);

      RelativeSelector {
        match_hint,
        bloom_hashes,
        ancestor_hashes,
        selector: selector.clone(),
      }
    })
    .collect::<Vec<_>>()
    .into_boxed_slice()
}

fn parse_slotted_pseudo_element<'i, 't>(
  parser: &mut Parser<'i, 't>,
  name: &cssparser::CowRcStr<'i>,
) -> std::result::Result<PseudoElement, ParseError<'i, SelectorParseErrorKind<'i>>> {
  // Per CSS Scoping, ::slotted() accepts a single <compound-selector>.
  // Reject selector lists (commas), combinators, and pseudo-elements inside the argument.
  let list = SelectorList::parse_disallow_pseudo(
    &PseudoClassParser,
    parser,
    selectors::parser::ParseRelative::No,
  )
  .map_err(|_| {
    parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
      name.clone(),
    ))
  })?;

  let selectors: Vec<_> = list.slice().iter().cloned().collect();
  if selectors.len() != 1 || selectors.iter().any(selector_has_combinators) {
    return Err(
      parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
        name.clone(),
      )),
    );
  }

  Ok(PseudoElement::Slotted(selectors.into_boxed_slice()))
}

fn parse_part_pseudo_element<'i, 't>(
  parser: &mut Parser<'i, 't>,
  name: &cssparser::CowRcStr<'i>,
) -> std::result::Result<PseudoElement, ParseError<'i, SelectorParseErrorKind<'i>>> {
  // Per CSS Shadow Parts, ::part() accepts one or more <ident> tokens, representing the
  // intersection of those part-name buckets. Reject anything other than a whitespace-separated
  // identifier list.
  parser.skip_whitespace();
  let mut idents: Vec<CssString> = Vec::new();
  let first =
    match parser.expect_ident() {
      Ok(first) => CssString::from(first.as_ref()),
      Err(_) => {
        return Err(parser.new_custom_error(
          SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name.clone()),
        ))
      }
    };
  idents.push(first);

  loop {
    parser.skip_whitespace();
    if parser.is_exhausted() {
      break;
    }
    let ident = match parser.expect_ident() {
      Ok(ident) => CssString::from(ident.as_ref()),
      Err(_) => {
        return Err(parser.new_custom_error(
          SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name.clone()),
        ))
      }
    };
    idents.push(ident);
  }

  idents.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
  idents.dedup_by(|a, b| a.as_str() == b.as_str());

  Ok(PseudoElement::Part(idents.into_boxed_slice()))
}

fn selector_has_combinators(selector: &Selector<FastRenderSelectorImpl>) -> bool {
  let mut iter = selector.iter();
  loop {
    while iter.next().is_some() {}
    match iter.next_sequence() {
      Some(_) => return true,
      None => return false,
    }
  }
}

/// Parse nth-child/nth-last-child expressions
fn parse_nth_with_of<'i, 't>(
  name: &cssparser::CowRcStr<'i>,
  parser: &mut Parser<'i, 't>,
) -> std::result::Result<
  (i32, i32, Option<SelectorList<FastRenderSelectorImpl>>),
  ParseError<'i, SelectorParseErrorKind<'i>>,
> {
  let (a, b) = parse_nth(parser).map_err(|_| {
    parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
      name.clone(),
    ))
  })?;

  let selector_list = if parser.is_exhausted() {
    None
  } else {
    Some(parse_of_selector_list(name, parser)?)
  };

  Ok((a, b, selector_list))
}

fn parse_of_selector_list<'i, 't>(
  name: &cssparser::CowRcStr<'i>,
  parser: &mut Parser<'i, 't>,
) -> std::result::Result<
  SelectorList<FastRenderSelectorImpl>,
  ParseError<'i, SelectorParseErrorKind<'i>>,
> {
  let ident = parser.expect_ident()?.clone();
  if !ident.eq_ignore_ascii_case("of") {
    return Err(
      parser.new_error(cssparser::BasicParseErrorKind::UnexpectedToken(
        Token::Ident(ident),
      )),
    );
  }

  SelectorList::parse(
    &PseudoClassParser,
    parser,
    selectors::parser::ParseRelative::No,
  )
  .map_err(|_| {
    parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
      name.clone(),
    ))
  })
}

fn parse_nth<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> std::result::Result<(i32, i32), ParseError<'i, ()>> {
  cssparser::parse_nth(parser).map_err(Into::into)
}

#[cfg(test)]
mod tests {
  use super::*;
  use cssparser::ParserInput;
  use cssparser::SourceLocation;
  use cssparser::ToCss;
  use precomputed_hash::PrecomputedHash;
  use selectors::context::QuirksMode;
  use selectors::parser::NonTSPseudoClass;
  use selectors::parser::ParseRelative;
  use selectors::parser::Parser as SelectorParser;

  fn parse(expr: &str) -> (i32, i32) {
    let mut input = ParserInput::new(expr);
    let mut parser = Parser::new(&mut input);
    parse_nth(&mut parser).expect("should parse nth expression")
  }

  fn parse_selector_list(selector_list: &str) -> SelectorList<FastRenderSelectorImpl> {
    let mut input = ParserInput::new(selector_list);
    let mut parser = Parser::new(&mut input);
    SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("selector list should parse")
  }

  #[test]
  fn parses_an_plus_b_syntax() {
    assert_eq!(parse("odd"), (2, 1));
    assert_eq!(parse("even"), (2, 0));
    assert_eq!(parse("2n+1"), (2, 1));
    assert_eq!(parse("-2n+3"), (-2, 3));
    assert_eq!(parse("n"), (1, 0));
    assert_eq!(parse("+n-1"), (1, -1));
    assert_eq!(parse("4"), (0, 4));
    assert_eq!(parse("-5"), (0, -5));
  }

  #[test]
  fn rejects_invalid_nth_expression() {
    let mut input = ParserInput::new("n+");
    let mut parser = Parser::new(&mut input);
    assert!(parse_nth(&mut parser).is_err());
  }

  #[test]
  fn parses_pseudo_classes_case_insensitively() {
    let parser = PseudoClassParser;
    let loc = SourceLocation { line: 0, column: 0 };
    let root = parser
      .parse_non_ts_pseudo_class(loc, cssparser::CowRcStr::from("RoOt"))
      .expect("root pseudo should parse");
    assert_eq!(root, PseudoClass::Root);

    let defined = parser
      .parse_non_ts_pseudo_class(loc, cssparser::CowRcStr::from("DeFiNeD"))
      .expect("defined pseudo should parse");
    assert_eq!(defined, PseudoClass::Defined);

    let target_within = parser
      .parse_non_ts_pseudo_class(loc, cssparser::CowRcStr::from("TARGET-WITHIN"))
      .expect("target-within pseudo should parse");
    assert_eq!(target_within, PseudoClass::TargetWithin);

    let mut input = ParserInput::new("2n+1");
    let mut css_parser = Parser::new(&mut input);
    let nth = parser
      .parse_non_ts_functional_pseudo_class(
        cssparser::CowRcStr::from("NTH-CHILD"),
        &mut css_parser,
        false,
      )
      .expect("nth-child pseudo should parse");
    assert!(matches!(nth, PseudoClass::NthChild(_, _, _)));
  }

  #[test]
  fn parses_lang_ranges_normalized() {
    use selectors::parser::Component;

    let list = parse_selector_list("div:lang(sr_Cyrl_RS)");
    let selector = list.slice().first().expect("one selector");
    let mut found = false;
    for component in selector.iter_raw_match_order() {
      if let Component::NonTSPseudoClass(PseudoClass::Lang(langs)) = component {
        assert_eq!(langs, &vec!["sr-cyrl-rs".to_string()]);
        found = true;
      }
    }
    assert!(found, "selector should contain :lang()");
    assert_eq!(selector.to_css_string(), "div:lang(sr-cyrl-rs)");
  }

  #[test]
  fn nth_child_of_specificity_includes_of_selector_list_specificity() {
    let pseudo_class_weight = PseudoClass::Root.specificity();
    let of_list = parse_selector_list("#target, .foo");
    let max_arg_spec = of_list
      .slice()
      .iter()
      .map(|selector| selector.specificity())
      .max()
      .unwrap_or(0);

    assert_eq!(
      PseudoClass::NthChild(0, 1, None).specificity(),
      pseudo_class_weight
    );
    assert_eq!(
      PseudoClass::NthChild(0, 1, Some(of_list)).specificity(),
      pseudo_class_weight + max_arg_spec
    );
  }

  #[test]
  fn moz_placeholder_shown_alias_serializes_to_standard_form() {
    for selector_text in [
      "input:-moz-placeholder-shown",
      "input:not(:-moz-placeholder-shown)",
      "input:focus, input:-moz-placeholder-shown, input:disabled",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }

    // The vendor pseudo-class should behave like a pure alias and serialize to the standard form.
    let list = parse_selector_list(".a:not(:-moz-placeholder-shown), .a:not(:placeholder-shown)");
    assert_eq!(list.slice().len(), 2);
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec![
        ".a:not(:placeholder-shown)".to_string(),
        ".a:not(:placeholder-shown)".to_string()
      ]
    );
  }

  #[test]
  fn nth_last_child_of_specificity_includes_of_selector_list_specificity() {
    let pseudo_class_weight = PseudoClass::Root.specificity();
    let of_list = parse_selector_list("#target, .foo");
    let max_arg_spec = of_list
      .slice()
      .iter()
      .map(|selector| selector.specificity())
      .max()
      .unwrap_or(0);

    assert_eq!(
      PseudoClass::NthLastChild(0, 1, None).specificity(),
      pseudo_class_weight
    );
    assert_eq!(
      PseudoClass::NthLastChild(0, 1, Some(of_list)).specificity(),
      pseudo_class_weight + max_arg_spec
    );
  }

  #[test]
  fn specificity_addition_clamps_each_component() {
    // The selectors crate represents specificity as three 10-bit components (A, B, C).
    // When adding pseudo-class specificity to an argument's specificity, B must not overflow into A.
    const MAX_10BIT: u32 = (1u32 << 10) - 1;

    let mut max_class_selector = String::new();
    for _ in 0..MAX_10BIT as usize {
      max_class_selector.push_str(".a");
    }
    let selectors = parse_selector_list(&max_class_selector);
    let arg_spec = selectors
      .slice()
      .first()
      .expect("one selector")
      .specificity();
    assert_eq!((arg_spec >> 20) & MAX_10BIT, 0);
    assert_eq!((arg_spec >> 10) & MAX_10BIT, MAX_10BIT);

    let nth_spec = PseudoClass::NthChild(0, 1, Some(selectors.clone())).specificity();
    assert_eq!(
      (nth_spec >> 20) & MAX_10BIT,
      0,
      "specificity overflow should not carry into the ID component"
    );
    assert_eq!((nth_spec >> 10) & MAX_10BIT, MAX_10BIT);

    let host_ctx_spec = PseudoClass::HostContext(selectors).specificity();
    assert_eq!((host_ctx_spec >> 20) & MAX_10BIT, 0);
    assert_eq!((host_ctx_spec >> 10) & MAX_10BIT, MAX_10BIT);
  }

  #[test]
  fn parses_pseudo_elements_case_insensitively() {
    let parser = PseudoClassParser;
    let loc = SourceLocation { line: 0, column: 0 };
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("BeFoRe"))
        .expect("before pseudo"),
      PseudoElement::Before
    );
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("FIRST-LINE"))
        .expect("first-line pseudo"),
      PseudoElement::FirstLine
    );
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("First-Letter"))
        .expect("first-letter pseudo"),
      PseudoElement::FirstLetter
    );
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("MARKER"))
        .expect("marker pseudo"),
      PseudoElement::Marker
    );
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("PlAcEhOlDeR"))
        .expect("placeholder pseudo"),
      PseudoElement::Placeholder
    );
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("-WebKit-SLIDER-THUMB"))
        .expect("slider thumb pseudo"),
      PseudoElement::SliderThumb
    );
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("-MOZ-RANGE-TRACK"))
        .expect("slider track pseudo"),
      PseudoElement::SliderTrack
    );
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("FOOTNOTE-CALL"))
        .expect("footnote-call pseudo"),
      PseudoElement::FootnoteCall
    );
    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("footnote-marker"))
        .expect("footnote-marker pseudo"),
      PseudoElement::FootnoteMarker
    );
  }

  #[test]
  fn parses_vendor_pseudo_element_aliases() {
    let parser = PseudoClassParser;
    let loc = SourceLocation { line: 0, column: 0 };
    for name in ["selection", "-moz-selection"] {
      assert_eq!(
        parser
          .parse_pseudo_element(loc, cssparser::CowRcStr::from(name))
          .unwrap(),
        PseudoElement::Selection,
        "{name} should map to ::selection"
      );
    }

    for name in [
      "-moz-list-bullet",
      "-moz-list-number",
      "-webkit-details-marker",
    ] {
      assert_eq!(
        parser
          .parse_pseudo_element(loc, cssparser::CowRcStr::from(name))
          .unwrap(),
        PseudoElement::Marker,
        "{name} should map to ::marker"
      );
    }

    for name in ["backdrop", "-webkit-backdrop", "-ms-backdrop"] {
      assert_eq!(
        parser
          .parse_pseudo_element(loc, cssparser::CowRcStr::from(name))
          .unwrap(),
        PseudoElement::Backdrop,
        "{name} should map to ::backdrop"
      );
    }

    for name in [
      "placeholder",
      "-webkit-input-placeholder",
      "-moz-placeholder",
      "-ms-input-placeholder",
    ] {
      assert_eq!(
        parser
          .parse_pseudo_element(loc, cssparser::CowRcStr::from(name))
          .unwrap(),
        PseudoElement::Placeholder,
        "{name} should map to ::placeholder"
      );
    }

    for name in ["file-selector-button", "-webkit-file-upload-button"] {
      assert_eq!(
        parser
          .parse_pseudo_element(loc, cssparser::CowRcStr::from(name))
          .unwrap(),
        PseudoElement::FileSelectorButton,
        "{name} should map to ::file-selector-button"
      );
    }

    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("-moz-focus-inner"))
        .unwrap(),
      PseudoElement::MozFocusInner
    );

    assert_eq!(
      parser
        .parse_pseudo_element(loc, cssparser::CowRcStr::from("-moz-focus-outer"))
        .unwrap(),
      PseudoElement::MozFocusOuter
    );

    for name in [
      "slider-thumb",
      "-webkit-slider-thumb",
      "-moz-range-thumb",
      "-ms-thumb",
    ] {
      assert_eq!(
        parser
          .parse_pseudo_element(loc, cssparser::CowRcStr::from(name))
          .unwrap(),
        PseudoElement::SliderThumb,
        "{name} should map to slider thumb"
      );
    }

    for name in [
      "slider-track",
      "-webkit-slider-runnable-track",
      "-moz-range-track",
      "-ms-track",
    ] {
      assert_eq!(
        parser
          .parse_pseudo_element(loc, cssparser::CowRcStr::from(name))
          .unwrap(),
        PseudoElement::SliderTrack,
        "{name} should map to slider track"
      );
    }

    assert_eq!(
      parser
        .parse_pseudo_element(
          loc,
          cssparser::CowRcStr::from("-webkit-search-cancel-button")
        )
        .unwrap(),
      PseudoElement::Vendor(CssString::from("-webkit-search-cancel-button"))
    );

    // `::-webkit-details-marker` should behave like an alias and serialize to the standard form.
    let list = parse_selector_list("summary::-webkit-details-marker, summary::marker");
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec!["summary::marker".to_string(), "summary::marker".to_string()]
    );

    // `::-webkit-file-upload-button` should behave like an alias and serialize to the standard form.
    let list =
      parse_selector_list("input::-webkit-file-upload-button, input::file-selector-button");
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec![
        "input::file-selector-button".to_string(),
        "input::file-selector-button".to_string()
      ]
    );
  }

  #[test]
  fn parses_form_control_vendor_pseudo_elements() {
    fn pseudo_for(selector: &str) -> PseudoElement {
      let mut input = ParserInput::new(selector);
      let mut parser = Parser::new(&mut input);
      let list =
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).expect("parse");
      list
        .slice()
        .first()
        .and_then(|sel| sel.pseudo_element().cloned())
        .expect("pseudo element")
    }

    assert_eq!(pseudo_for("input::placeholder"), PseudoElement::Placeholder);
    assert_eq!(
      pseudo_for("input::-webkit-input-placeholder"),
      PseudoElement::Placeholder
    );
    assert_eq!(
      pseudo_for("input::-moz-placeholder"),
      PseudoElement::Placeholder
    );
    assert_eq!(
      pseudo_for("input::-ms-input-placeholder"),
      PseudoElement::Placeholder
    );

    assert_eq!(
      pseudo_for("input::-webkit-slider-thumb"),
      PseudoElement::SliderThumb
    );
    assert_eq!(pseudo_for("input::slider-thumb"), PseudoElement::SliderThumb);
    assert_eq!(
      pseudo_for("input::-moz-range-thumb"),
      PseudoElement::SliderThumb
    );
    assert_eq!(pseudo_for("input::-ms-thumb"), PseudoElement::SliderThumb);

    assert_eq!(
      pseudo_for("input::-webkit-slider-runnable-track"),
      PseudoElement::SliderTrack
    );
    assert_eq!(pseudo_for("input::slider-track"), PseudoElement::SliderTrack);
    assert_eq!(
      pseudo_for("input::-moz-range-track"),
      PseudoElement::SliderTrack
    );
    assert_eq!(pseudo_for("input::-ms-track"), PseudoElement::SliderTrack);

    assert_eq!(
      pseudo_for("input::file-selector-button"),
      PseudoElement::FileSelectorButton
    );
    assert_eq!(
      pseudo_for("input::-webkit-file-upload-button"),
      PseudoElement::FileSelectorButton
    );
  }

  #[test]
  fn relative_selector_bloom_hashes_are_precomputed() {
    let mut input = ParserInput::new("span.foo #bar[data-Thing]");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    assert_eq!(selectors.len(), 1);
    let selector = &selectors[0];

    let hash =
      |value: &str| CssString::from(value).precomputed_hash() & selectors::bloom::BLOOM_HASH_MASK;

    // The selectors crate stores bloom hashes in raw parse order, which is an
    // implementation detail. Compare as sets (sorted) so this regression test
    // continues to validate the presence of expected hashes without coupling to
    // internal ordering.
    //
    // Attribute selector names are case-insensitive in HTML, and the DOM stores attribute names
    // lowercased (via html5ever). Match the bloom-pruning behavior by hashing the lowercase form.
    // Only include hashes from the selector's rightmost compound selector to avoid false-negative
    // pruning when relative selectors can match ancestors outside the :has() anchor subtree
    // (e.g. via `:is()` breakouts like `:is(.a .b) .c`).
    let mut expected_no_quirks = vec![hash("bar"), hash("data-thing")];
    expected_no_quirks.sort_unstable();
    let mut actual_no_quirks = selector
      .bloom_hashes
      .hashes_for_mode(QuirksMode::NoQuirks)
      .to_vec();
    actual_no_quirks.sort_unstable();
    assert_eq!(actual_no_quirks, expected_no_quirks);

    let mut expected_quirks = vec![hash("data-thing")];
    expected_quirks.sort_unstable();
    let mut actual_quirks = selector
      .bloom_hashes
      .hashes_for_mode(QuirksMode::Quirks)
      .to_vec();
    actual_quirks.sort_unstable();
    assert_eq!(actual_quirks, expected_quirks);
  }

  #[test]
  fn parses_single_colon_before_as_pseudo_element() {
    let mut input = ParserInput::new("div:before");
    let mut css_parser = Parser::new(&mut input);
    let selector_list = SelectorList::parse(
      &PseudoClassParser,
      &mut css_parser,
      selectors::parser::ParseRelative::No,
    );

    let list = selector_list.expect("should parse selector list");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(selector.pseudo_element(), Some(&PseudoElement::Before));
  }

  #[test]
  fn parses_vendor_placeholder_pseudo_classes() {
    for selector in [
      ".form-floating > .form-control:not(:-moz-placeholder)",
      ".form-floating > .form-control:not(:-ms-input-placeholder)",
      ".x:-moz-placeholder-shown",
    ] {
      let mut input = ParserInput::new(selector);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "selector should parse: {selector}"
      );
    }
  }

  #[test]
  fn parses_selector_list_mixing_vendor_and_standard_placeholder_pseudos() {
    let mut input = ParserInput::new(".a:not(:-moz-placeholder), .a:not(:placeholder-shown) {}");
    let mut parser = Parser::new(&mut input);
    assert!(parser
      .parse_until_before(cssparser::Delimiter::CurlyBracketBlock, |nested| {
        SelectorList::parse(&PseudoClassParser, nested, ParseRelative::No)
      })
      .is_ok());
  }

  #[test]
  fn to_css_serializes_new_pseudo_classes() {
    assert_eq!(PseudoClass::FirstOfType.to_css_string(), ":first-of-type");
    assert_eq!(PseudoClass::LastOfType.to_css_string(), ":last-of-type");
    assert_eq!(PseudoClass::OnlyOfType.to_css_string(), ":only-of-type");
    assert_eq!(PseudoClass::Empty.to_css_string(), ":empty");
    assert_eq!(PseudoClass::Defined.to_css_string(), ":defined");
    assert_eq!(
      PseudoClass::NthOfType(2, 1).to_css_string(),
      ":nth-of-type(2n+1)"
    );
    assert_eq!(
      PseudoClass::NthLastOfType(-1, 3).to_css_string(),
      ":nth-last-of-type(-1n+3)"
    );
    assert_eq!(
      PseudoClass::Lang(vec!["en".into(), "fr-ca".into()]).to_css_string(),
      ":lang(en, fr-ca)"
    );
    assert_eq!(
      PseudoClass::Dir(TextDirection::Ltr).to_css_string(),
      ":dir(ltr)"
    );
    assert_eq!(PseudoClass::AnyLink.to_css_string(), ":any-link");
    assert_eq!(PseudoClass::Target.to_css_string(), ":target");
    assert_eq!(PseudoClass::TargetWithin.to_css_string(), ":target-within");
    assert_eq!(PseudoClass::Scope.to_css_string(), ":scope");
    assert_eq!(PseudoClass::Disabled.to_css_string(), ":disabled");
    assert_eq!(PseudoClass::Enabled.to_css_string(), ":enabled");
    assert_eq!(PseudoClass::Required.to_css_string(), ":required");
    assert_eq!(PseudoClass::Optional.to_css_string(), ":optional");
    assert_eq!(PseudoClass::Valid.to_css_string(), ":valid");
    assert_eq!(PseudoClass::Invalid.to_css_string(), ":invalid");
    assert_eq!(PseudoClass::UserValid.to_css_string(), ":user-valid");
    assert_eq!(PseudoClass::UserInvalid.to_css_string(), ":user-invalid");
    assert_eq!(PseudoClass::InRange.to_css_string(), ":in-range");
    assert_eq!(PseudoClass::OutOfRange.to_css_string(), ":out-of-range");
    assert_eq!(PseudoClass::Indeterminate.to_css_string(), ":indeterminate");
    assert_eq!(PseudoClass::Default.to_css_string(), ":default");
    assert_eq!(PseudoClass::FocusWithin.to_css_string(), ":focus-within");
    assert_eq!(PseudoClass::FocusVisible.to_css_string(), ":focus-visible");
    assert_eq!(PseudoClass::Fullscreen.to_css_string(), ":fullscreen");
    assert_eq!(PseudoClass::Open.to_css_string(), ":open");
    assert_eq!(PseudoClass::Modal.to_css_string(), ":modal");
    assert_eq!(PseudoClass::PopoverOpen.to_css_string(), ":popover-open");
    assert_eq!(PseudoClass::ReadOnly.to_css_string(), ":read-only");
    assert_eq!(PseudoClass::ReadWrite.to_css_string(), ":read-write");
    assert_eq!(
      PseudoClass::PlaceholderShown.to_css_string(),
      ":placeholder-shown"
    );
    assert_eq!(
      PseudoClass::WebkitInputPlaceholder.to_css_string(),
      ":-webkit-input-placeholder"
    );
    assert_eq!(
      PseudoClass::MsInputPlaceholder.to_css_string(),
      ":-ms-input-placeholder"
    );
    assert_eq!(
      PseudoClass::MozPlaceholder.to_css_string(),
      ":-moz-placeholder"
    );
    assert_eq!(PseudoClass::Autofill.to_css_string(), ":autofill");
    assert_eq!(
      PseudoClass::MozUiInvalid.to_css_string(),
      ":-moz-ui-invalid"
    );
    assert_eq!(PseudoClass::MozFocusring.to_css_string(), ":-moz-focusring");
    assert_eq!(
      PseudoClass::Vendor(CssString::from("-moz-broken")).to_css_string(),
      ":-moz-broken"
    );
  }

  #[test]
  fn parses_vendor_form_control_pseudo_classes() {
    let mut input = ParserInput::new(
      "input:-ms-input-placeholder, input:-moz-placeholder, input:-moz-placeholder-shown, input:-webkit-autofill, input:-moz-autofill, input:-moz-ui-invalid, input:-moz-focusring",
    );
    let mut parser = Parser::new(&mut input);
    assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok());
  }

  #[test]
  fn parses_user_valid_and_invalid_pseudo_classes() {
    for selector_text in [
      "input:user-invalid",
      "input:user-valid",
      "input:not(:user-invalid)",
      "input:focus, input:user-invalid, input:user-valid, input:disabled",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }
  }

  #[test]
  fn vendor_form_control_pseudo_classes_do_not_invalidate_selector_lists() {
    let mut input = ParserInput::new(
      "input:focus, input:-ms-input-placeholder, input:-moz-placeholder, input:-moz-placeholder-shown, input:-webkit-autofill, input:-moz-autofill, input:-moz-ui-invalid, input:-moz-focusring, input:disabled",
    );
    let mut parser = Parser::new(&mut input);
    assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok());
  }

  #[test]
  fn parses_moz_placeholder_shown_alias_in_various_selector_contexts() {
    for selector_text in [
      "input:-moz-placeholder-shown",
      "input:not(:-moz-placeholder-shown)",
      "input:focus, input:-moz-placeholder-shown, input:disabled",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }
  }

  #[test]
  fn parses_fullscreen_pseudo_class_aliases() {
    for selector_text in [
      "video:fullscreen",
      "video:-webkit-full-screen",
      "video:-moz-full-screen",
      "video:-ms-fullscreen",
      "video:fullscreen, video:-webkit-full-screen, video",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }

    // Vendor spellings should behave like aliases and serialize to the standard form.
    let list = parse_selector_list("video:-webkit-full-screen, video:fullscreen");
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec![
        "video:fullscreen".to_string(),
        "video:fullscreen".to_string()
      ]
    );
  }

  #[test]
  fn parses_top_layer_state_pseudo_classes() {
    for selector_text in [
      ".tooltip[popover]:popover-open",
      ".tooltip[popover]:popover-open, .tooltip[popover].fallback",
      "dialog:modal",
      "body:has(>dialog:modal[open])",
      "details:open > summary",
      "details-dialog:defined, details-dialog:not(:defined)",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }

    let list = parse_selector_list(".tooltip[popover]:popover-open, .tooltip[popover].fallback");
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec![
        ".tooltip[popover]:popover-open".to_string(),
        ".tooltip[popover].fallback".to_string()
      ]
    );
  }

  #[test]
  fn parses_unknown_vendor_pseudo_classes() {
    for selector_text in [
      "a:-moz-broken",
      "a:-webkit-nonexistent-pseudo",
      "a:-ms-nonexistent-pseudo",
      "a:-o-nonexistent-pseudo",
      "a:-khtml-nonexistent-pseudo",
      "a, a:-moz-broken",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }

    let list = parse_selector_list("a:-webkit-nonexistent-pseudo, a:-moz-broken");
    assert_eq!(
      list.to_css_string(),
      "a:-webkit-nonexistent-pseudo, a:-moz-broken"
    );
  }

  #[test]
  fn parses_any_link_vendor_aliases() {
    for selector_text in [
      "a:any-link",
      "a:-webkit-any-link",
      "a:-moz-any-link",
      "a:any-link, a:-webkit-any-link, a",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }

    // Vendor spellings should behave like aliases and serialize to the standard form.
    let list = parse_selector_list("a:-webkit-any-link, a:-moz-any-link, a:any-link");
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec![
        "a:any-link".to_string(),
        "a:any-link".to_string(),
        "a:any-link".to_string()
      ]
    );
  }

  #[test]
  fn parses_vendor_any_functional_pseudo_as_is_alias() {
    for selector_text in [
      "div:is(.a, #b)",
      "div:matches(.a, #b)",
      "div:-webkit-any(.a, #b)",
      "div:-moz-any(.a, #b)",
      "div:-webkit-any(.a, #b), div",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }

    // Legacy spellings should behave like aliases and serialize to `:is(...)`.
    let list =
      parse_selector_list("div:matches(.a, #b), div:-webkit-any(.a, #b), div:-moz-any(.a, #b)");
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec![
        "div:is(.a, #b)".to_string(),
        "div:is(.a, #b)".to_string(),
        "div:is(.a, #b)".to_string()
      ]
    );
  }

  #[test]
  fn parses_shadow_pseudo_elements() {
    let mut input = ParserInput::new("div::slotted(.a)");
    let mut parser = Parser::new(&mut input);
    assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok());

    let mut input = ParserInput::new("button::part(name)");
    let mut parser = Parser::new(&mut input);
    assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok());
  }

  #[test]
  fn parses_moz_focus_inner_pseudo_element_selectors() {
    for selector_text in [
      "button::-moz-focus-inner",
      "input::-moz-focus-inner",
      "button::-moz-focus-inner, input::-moz-focus-inner",
      "button:-moz-focus-inner, input:-moz-focus-inner",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }
  }

  #[test]
  fn parses_moz_focus_outer_pseudo_element_selectors() {
    for selector_text in [
      "button::-moz-focus-outer",
      "input::-moz-focus-outer",
      "button::-moz-focus-outer, input::-moz-focus-outer",
      "button:-moz-focus-outer, input:-moz-focus-outer",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }
  }

  #[test]
  fn parses_selection_pseudo_element_selectors() {
    for selector_text in [
      ".CodeMirror-line::selection",
      ".CodeMirror-line::-moz-selection",
      ".CodeMirror-line:selection",
      ".CodeMirror-line:-moz-selection",
      ".CodeMirror-line::selection, .CodeMirror-line::-moz-selection, .CodeMirror-line",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }

    // Vendor spelling should behave like an alias and serialize to the standard form.
    let list = parse_selector_list(".a::-moz-selection, .a::selection");
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec![".a::selection".to_string(), ".a::selection".to_string()]
    );
  }

  #[test]
  fn parses_unknown_vendor_pseudo_element_selectors() {
    for selector_text in [
      "input::-webkit-inner-spin-button",
      "input::-webkit-search-cancel-button",
      "input::-ms-clear",
      "progress::-moz-progress-bar",
      "progress::-khtml-progress-bar",
      "dialog::-webkit-backdrop, dialog::backdrop",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }

    let list = parse_selector_list("input::-webkit-search-cancel-button");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(
      selector.pseudo_element(),
      Some(&PseudoElement::Vendor(CssString::from(
        "-webkit-search-cancel-button"
      )))
    );
    assert_eq!(
      selector.to_css_string(),
      "input::-webkit-search-cancel-button"
    );

    // `::-webkit-backdrop` should behave like an alias and serialize to the standard form.
    let list = parse_selector_list("dialog::-webkit-backdrop, dialog::backdrop");
    let selectors: Vec<String> = list.slice().iter().map(|sel| sel.to_css_string()).collect();
    assert_eq!(
      selectors,
      vec![
        "dialog::backdrop".to_string(),
        "dialog::backdrop".to_string()
      ]
    );
  }

  #[test]
  fn parses_placeholder_pseudo_elements() {
    let mut input = ParserInput::new("input::-ms-input-placeholder, input::placeholder");
    let mut parser = Parser::new(&mut input);
    assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok());
  }

  #[test]
  fn parses_vendor_placeholder_selector_list() {
    let mut input = ParserInput::new(
      "input::-webkit-input-placeholder, input::-moz-placeholder, input::-ms-input-placeholder, input::placeholder",
    );
    let mut parser = Parser::new(&mut input);
    assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok());
  }

  #[test]
  fn parses_part_with_multiple_names() {
    let parse_part = |selector_text: &str| {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list =
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).expect("parse");
      list
        .slice()
        .first()
        .expect("selector")
        .pseudo_element()
        .expect("part pseudo")
        .clone()
    };

    let a = parse_part("button::part(name badge)");
    let b = parse_part("button::part(badge name)");
    assert_eq!(a, b, "part names should be order-insensitive");
    assert_eq!(a.to_css_string(), "::part(badge name)");
  }

  #[test]
  fn rejects_non_compound_slotted_selector() {
    let mut input = ParserInput::new("div::slotted(.a .b)");
    let mut parser = Parser::new(&mut input);
    assert!(SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_err());
  }

  #[test]
  fn rejects_slotted_selector_list_and_pseudo_elements() {
    for selector_text in ["div::slotted(.a, .b)", "div::slotted(::before)"] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_err(),
        "{selector_text} should be rejected"
      );
    }
  }

  #[test]
  fn to_css_serializes_pseudo_elements() {
    let mut input = ParserInput::new(".foo");
    let mut parser = Parser::new(&mut input);
    let selector_list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("should parse selector");
    let selector = selector_list
      .slice()
      .first()
      .expect("expected selector")
      .clone();
    let slotted = PseudoElement::Slotted(vec![selector].into_boxed_slice());
    assert_eq!(slotted.to_css_string(), "::slotted(.foo)");

    let part = PseudoElement::Part(vec![CssString::from("name")].into_boxed_slice());
    assert_eq!(part.to_css_string(), "::part(name)");

    assert_eq!(PseudoElement::Placeholder.to_css_string(), "::placeholder");
    assert_eq!(
      PseudoElement::FileSelectorButton.to_css_string(),
      "::file-selector-button"
    );
    assert_eq!(PseudoElement::Selection.to_css_string(), "::selection");
    assert_eq!(
      PseudoElement::SliderThumb.to_css_string(),
      "::-webkit-slider-thumb"
    );
    assert_eq!(
      PseudoElement::SliderTrack.to_css_string(),
      "::-webkit-slider-runnable-track"
    );
    assert_eq!(
      PseudoElement::MozFocusOuter.to_css_string(),
      "::-moz-focus-outer"
    );
    assert_eq!(
      PseudoElement::Vendor(CssString::from("-webkit-search-cancel-button")).to_css_string(),
      "::-webkit-search-cancel-button"
    );
  }

  #[test]
  fn parses_form_control_pseudo_element_selectors() {
    let mut input = ParserInput::new("input::placeholder");
    let mut parser = Parser::new(&mut input);
    let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("parse placeholder selector");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(selector.pseudo_element(), Some(&PseudoElement::Placeholder));

    let mut input = ParserInput::new("input::file-selector-button");
    let mut parser = Parser::new(&mut input);
    let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("parse file-selector-button selector");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(
      selector.pseudo_element(),
      Some(&PseudoElement::FileSelectorButton)
    );

    let mut input = ParserInput::new(".range::-webkit-slider-thumb");
    let mut parser = Parser::new(&mut input);
    let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("parse webkit slider thumb selector");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(selector.pseudo_element(), Some(&PseudoElement::SliderThumb));

    let mut input = ParserInput::new(".range::-moz-range-track");
    let mut parser = Parser::new(&mut input);
    let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("parse moz range track selector");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(selector.pseudo_element(), Some(&PseudoElement::SliderTrack));

    let mut input = ParserInput::new("input::file-selector-button");
    let mut parser = Parser::new(&mut input);
    let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("parse file-selector-button selector");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(
      selector.pseudo_element(),
      Some(&PseudoElement::FileSelectorButton)
    );
  }

  #[test]
  fn parses_single_colon_form_control_pseudo_element_selectors() {
    for selector_text in [
      "input:placeholder",
      "input:-webkit-input-placeholder",
      "input:-moz-placeholder",
      "input:-ms-input-placeholder",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
        .expect("parse placeholder selector");
      let selector = list.slice().first().expect("one selector");
      assert_eq!(
        selector.pseudo_element(),
        Some(&PseudoElement::Placeholder),
        "{selector_text} should parse as ::placeholder"
      );
    }

    for (selector_text, canonical) in [
      ("input:file-selector-button", "input::file-selector-button"),
      (
        "input:-webkit-file-upload-button",
        "input::file-selector-button",
      ),
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list =
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).expect("parse");
      let selector = list.slice().first().expect("one selector");
      assert_eq!(
        selector.pseudo_element(),
        Some(&PseudoElement::FileSelectorButton),
        "{selector_text} should parse as ::file-selector-button"
      );
      assert_eq!(selector.to_css_string(), canonical);
    }

    for selector_text in [
      ".range:-webkit-slider-thumb",
      ".range:-moz-range-thumb",
      ".range:-ms-thumb",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
        .expect("parse slider thumb selector");
      let selector = list.slice().first().expect("one selector");
      assert_eq!(
        selector.pseudo_element(),
        Some(&PseudoElement::SliderThumb),
        "{selector_text} should parse as slider thumb"
      );
    }

    for selector_text in [
      ".range:-webkit-slider-runnable-track",
      ".range:-moz-range-track",
      ".range:-ms-track",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
        .expect("parse slider track selector");
      let selector = list.slice().first().expect("one selector");
      assert_eq!(
        selector.pseudo_element(),
        Some(&PseudoElement::SliderTrack),
        "{selector_text} should parse as slider track"
      );
    }

    for selector_text in [
      "input:file-selector-button",
      "input:-webkit-file-upload-button",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
        .expect("parse file selector button selector");
      let selector = list.slice().first().expect("one selector");
      assert_eq!(
        selector.pseudo_element(),
        Some(&PseudoElement::FileSelectorButton),
        "{selector_text} should parse as ::file-selector-button"
      );
    }

    for (selector_text, canonical) in [
      ("summary:-webkit-details-marker", "summary::marker"),
      ("li:-moz-list-bullet", "li::marker"),
      ("li:-moz-list-number", "li::marker"),
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
        .expect("parse marker selector");
      let selector = list.slice().first().expect("one selector");
      assert_eq!(
        selector.pseudo_element(),
        Some(&PseudoElement::Marker),
        "{selector_text} should parse as ::marker"
      );
      assert_eq!(selector.to_css_string(), canonical);
    }

    for (selector_text, canonical) in [
      ("dialog:-webkit-backdrop", "dialog::backdrop"),
      ("dialog:-ms-backdrop", "dialog::backdrop"),
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
        .expect("parse backdrop selector");
      let selector = list.slice().first().expect("one selector");
      assert_eq!(
        selector.pseudo_element(),
        Some(&PseudoElement::Backdrop),
        "{selector_text} should parse as ::backdrop"
      );
      assert_eq!(selector.to_css_string(), canonical);
    }

    for (selector_text, canonical, pseudo) in [
      ("li:marker", "li::marker", PseudoElement::Marker),
      (
        "dialog:backdrop",
        "dialog::backdrop",
        PseudoElement::Backdrop,
      ),
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      let list =
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).expect("parse");
      let selector = list.slice().first().expect("one selector");
      assert_eq!(
        selector.pseudo_element(),
        Some(&pseudo),
        "{selector_text} should parse as pseudo-element"
      );
      assert_eq!(selector.to_css_string(), canonical);
    }

    let mut input = ParserInput::new("div:part(foo)");
    let mut parser = Parser::new(&mut input);
    let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("parse part selector");
    let selector = list.slice().first().expect("one selector");
    assert!(matches!(
      selector.pseudo_element(),
      Some(PseudoElement::Part(_))
    ));
    assert_eq!(selector.to_css_string(), "div::part(foo)");

    let mut input = ParserInput::new("slot:slotted(span)");
    let mut parser = Parser::new(&mut input);
    let list = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("parse slotted selector");
    let selector = list.slice().first().expect("one selector");
    assert!(matches!(
      selector.pseudo_element(),
      Some(PseudoElement::Slotted(_))
    ));
    assert_eq!(selector.to_css_string(), "slot::slotted(span)");
  }

  #[test]
  fn parses_legacy_vendor_placeholder_pseudos_inside_not() {
    for selector_text in [
      "input:not(:-webkit-input-placeholder)",
      "input:not(:-moz-placeholder)",
      "input:not(:-ms-input-placeholder)",
    ] {
      let mut input = ParserInput::new(selector_text);
      let mut parser = Parser::new(&mut input);
      assert!(
        SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_ok(),
        "{selector_text} should parse"
      );
    }
  }

  #[test]
  fn parses_placeholder_pseudo_element_aliases() {
    let list = parse_selector_list("input::-webkit-input-placeholder");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(selector.pseudo_element(), Some(&PseudoElement::Placeholder));
    assert_eq!(selector.to_css_string(), "input::placeholder");

    let list = parse_selector_list("input::-moz-placeholder");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(selector.pseudo_element(), Some(&PseudoElement::Placeholder));
    assert_eq!(selector.to_css_string(), "input::placeholder");
  }

  #[test]
  fn parses_file_selector_button_pseudo_element_aliases() {
    let list = parse_selector_list("input::-webkit-file-upload-button");
    let selector = list.slice().first().expect("one selector");
    assert_eq!(
      selector.pseudo_element(),
      Some(&PseudoElement::FileSelectorButton)
    );
    assert_eq!(selector.to_css_string(), "input::file-selector-button");
  }

  #[test]
  fn parses_placeholder_pseudo_element_selector_list_with_vendor_variants() {
    let list = parse_selector_list(
      "input::-webkit-input-placeholder, input::-moz-placeholder, input::placeholder",
    );
    assert_eq!(list.len(), 3);
    for selector in list.slice() {
      assert_eq!(selector.pseudo_element(), Some(&PseudoElement::Placeholder));
    }
    assert_eq!(
      list.to_css_string(),
      "input::placeholder, input::placeholder, input::placeholder"
    );
  }
}
