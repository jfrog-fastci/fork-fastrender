use crate::css::selectors::FastRenderSelectorImpl;
use crate::css::selectors::ExportedPartTarget;
use crate::css::selectors::PartExportMap;
use crate::css::selectors::PseudoClass;
use crate::css::selectors::PseudoElement;
use crate::css::selectors::SlotAssignmentMap;
use crate::css::selectors::TextDirection;
use crate::css::types::CssString;
use crate::error::Error;
use crate::error::ParseError;
use crate::error::RenderStage;
use crate::error::Result;
use crate::render_control::check_active_periodic;
use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::QuirksMode as HtmlQuirksMode;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::ParseOpts;
use markup5ever_rcdom::Handle;
use markup5ever_rcdom::NodeData;
use markup5ever_rcdom::RcDom;
use rustc_hash::FxHashSet;
use selectors::attr::AttrSelectorOperation;
use selectors::attr::CaseSensitivity;
use selectors::bloom::BloomFilter;
use selectors::context::QuirksMode;
use selectors::matching::matches_selector;
use selectors::matching::selector_may_match;
use selectors::matching::MatchingContext;
use selectors::parser::RelativeSelector;
use selectors::parser::Selector;
use selectors::relative_selector::cache::RelativeSelectorCachedMatch;
use selectors::Element;
use selectors::OpaqueElement;
use serde::{Deserialize, Serialize};
use std::borrow::{Borrow, Cow};
use std::cell::{Cell, RefCell, RefMut};
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};
use std::io;
use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::thread_local;
use std::time::Instant;
use unicode_bidi::bidi_class;

pub const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";
pub const SVG_NAMESPACE: &str = "http://www.w3.org/2000/svg";
pub const MATHML_NAMESPACE: &str = "http://www.w3.org/1998/Math/MathML";

const RELATIVE_SELECTOR_DEADLINE_STRIDE: usize = 64;
// Upper bound on how deep we track ancestor hashes in the counting bloom filter used while
// evaluating `:has()` relative selectors.
//
// Beyond this depth the bloom filter becomes expensive to maintain and is likely to saturate (the
// u8-backed counters cap at 0xff and can't be decremented once saturated), which reduces pruning
// value. In those cases we disable ancestor bloom pruning entirely to avoid false negatives from
// incomplete ancestry tracking.
const RELATIVE_SELECTOR_ANCESTOR_BLOOM_MAX_DEPTH: usize = 240;
const NTH_DEADLINE_STRIDE: usize = 64;
const DOM_PARSE_READ_DEADLINE_STRIDE: usize = 1;
const DOM_PARSE_NODE_DEADLINE_STRIDE: usize = 1024;
const SHADOW_MAP_DEADLINE_STRIDE: usize = 1024;
const DOM_PARSE_READ_MAX_CHUNK_BYTES: usize = 16 * 1024;

#[cfg(test)]
thread_local! {
  static NTH_OF_CACHE_POPULATIONS: AtomicU64 = const { AtomicU64::new(0) };
}

/// Controls whether non-standard DOM compatibility mutations are applied while parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DomCompatibilityMode {
  /// Parse the DOM without any FastRender-specific mutations.
  Standard,

  /// Apply compatibility mutations to mimic JS-driven class flips in static renders.
  Compatibility,
}

impl Default for DomCompatibilityMode {
  fn default() -> Self {
    Self::Standard
  }
}

/// Options for DOM parsing.
#[derive(Debug, Clone, Copy)]
pub struct DomParseOptions {
  /// Whether to enable HTML parsing semantics that assume JavaScript is enabled.
  ///
  /// This maps directly to `html5ever::tree_builder::TreeBuilderOpts::scripting_enabled` and
  /// affects parsing of elements such as `<noscript>`.
  pub scripting_enabled: bool,
  /// Optional compatibility mutations applied after HTML parsing.
  pub compatibility_mode: DomCompatibilityMode,
}

impl Default for DomParseOptions {
  fn default() -> Self {
    Self {
      scripting_enabled: false,
      compatibility_mode: DomCompatibilityMode::Standard,
    }
  }
}

impl DomParseOptions {
  /// Construct parse options with explicit scripting mode.
  pub fn with_scripting_enabled(scripting_enabled: bool) -> Self {
    Self {
      scripting_enabled,
      ..Default::default()
    }
  }

  /// Enable JavaScript parsing semantics.
  ///
  /// Equivalent to `DomParseOptions::with_scripting_enabled(true)`.
  pub fn javascript_enabled() -> Self {
    Self::with_scripting_enabled(true)
  }

  /// Enable compatibility DOM mutations (e.g., JS-managed class flips).
  pub fn compatibility() -> Self {
    Self {
      compatibility_mode: DomCompatibilityMode::Compatibility,
      ..Default::default()
    }
  }
}

mod scripting_parser;
#[allow(deprecated)]
pub use scripting_parser::{parse_html_with_scripting, ScriptToken};

pub(crate) mod forms_validation;

#[derive(Debug, Default, Clone)]
pub(crate) struct DomParseDiagnostics {
  pub html5ever_ms: f64,
  pub convert_ms: f64,
  pub shadow_attach_ms: f64,
  pub compat_ms: f64,
}

static DOM_PARSE_DIAGNOSTICS: OnceLock<Mutex<DomParseDiagnostics>> = OnceLock::new();

thread_local! {
  static DOM_PARSE_DIAGNOSTICS_ENABLED: Cell<bool> = const { Cell::new(false) };
}

fn dom_parse_diagnostics_cell() -> &'static Mutex<DomParseDiagnostics> {
  DOM_PARSE_DIAGNOSTICS.get_or_init(|| Mutex::new(DomParseDiagnostics::default()))
}

pub(crate) fn enable_dom_parse_diagnostics() {
  DOM_PARSE_DIAGNOSTICS_ENABLED.with(|enabled| enabled.set(true));
  if let Ok(mut diag) = dom_parse_diagnostics_cell().lock() {
    *diag = DomParseDiagnostics::default();
  }
}

pub(crate) fn take_dom_parse_diagnostics() -> Option<DomParseDiagnostics> {
  let was_enabled = DOM_PARSE_DIAGNOSTICS_ENABLED.with(|enabled| {
    let prev = enabled.get();
    enabled.set(false);
    prev
  });
  if !was_enabled {
    return None;
  }

  dom_parse_diagnostics_cell()
    .lock()
    .ok()
    .map(|diag| diag.clone())
}

fn dom_parse_diagnostics_enabled() -> bool {
  DOM_PARSE_DIAGNOSTICS_ENABLED.with(|enabled| enabled.get())
}

fn dom_parse_diagnostics_timer() -> Option<Instant> {
  dom_parse_diagnostics_enabled().then(Instant::now)
}

fn with_dom_parse_diagnostics(f: impl FnOnce(&mut DomParseDiagnostics)) {
  if !dom_parse_diagnostics_enabled() {
    return;
  }

  if let Ok(mut diag) = dom_parse_diagnostics_cell().lock() {
    f(&mut diag);
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowRootMode {
  Open,
  Closed,
}

#[derive(Debug, Clone)]
pub struct DomNode {
  pub node_type: DomNodeType,
  pub children: Vec<DomNode>,
}

impl Drop for DomNode {
  fn drop(&mut self) {
    // Dropping a deeply-nested `DomNode` tree via Rust's default recursive drop can overflow the
    // stack (e.g. degenerate 100k-depth trees from real pages or fuzzing). Drop children
    // iteratively by draining them into an explicit stack so each node is dropped with an empty
    // `children` vec.
    if self.children.is_empty() {
      return;
    }

    let mut stack: Vec<DomNode> = std::mem::take(&mut self.children);
    while let Some(mut node) = stack.pop() {
      stack.append(&mut node.children);
      // `node` is dropped here with an empty `children` vec, so this `Drop` implementation becomes
      // a cheap no-op for all non-root nodes in the iterative drain.
    }
  }
}

/// Mapping between light DOM nodes and their assigned slots within shadow roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotAssignment {
  /// For each shadow root, the slots it exposes and their assigned node ids.
  pub shadow_to_slots: HashMap<usize, HashMap<String, Vec<usize>>>,
  /// For each slot element id, the ordered list of assigned node ids.
  pub slot_to_nodes: HashMap<usize, Vec<usize>>,
  /// For each assigned node id, which slot it was assigned to.
  pub node_to_slot: HashMap<usize, AssignedSlot>,
}

/// Slot destination for an assigned light DOM node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedSlot {
  pub slot_name: String,
  pub slot_node_id: usize,
  pub shadow_root_id: usize,
}

#[derive(Debug, Clone)]
pub enum DomNodeType {
  Document {
    quirks_mode: QuirksMode,
  },
  ShadowRoot {
    mode: ShadowRootMode,
    delegates_focus: bool,
  },
  Slot {
    namespace: String,
    attributes: Vec<(String, String)>,
    assigned: bool,
  },
  Element {
    tag_name: String,
    namespace: String,
    attributes: Vec<(String, String)>,
  },
  Text {
    content: String,
  },
}

thread_local! {
    static TARGET_FRAGMENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn with_target_fragment<R, F: FnOnce() -> R>(target: Option<&str>, f: F) -> R {
  TARGET_FRAGMENT.with(|slot| {
    let previous = slot.borrow_mut().take();
    if let Some(t) = target {
      let without_hash = t.strip_prefix('#').unwrap_or(t);
      let decoded = percent_encoding::percent_decode_str(without_hash)
        .decode_utf8_lossy()
        .into_owned();
      *slot.borrow_mut() = Some(decoded);
    }
    let result = f();
    *slot.borrow_mut() = previous;
    result
  })
}

fn current_target_fragment() -> Option<String> {
  TARGET_FRAGMENT.with(|slot| slot.borrow().clone())
}

static SELECTOR_BLOOM_ENV_INITIALIZED: OnceLock<()> = OnceLock::new();
static SELECTOR_BLOOM_ENABLED: AtomicBool = AtomicBool::new(true);
static ANCESTOR_BLOOM_ENV_INITIALIZED: OnceLock<()> = OnceLock::new();
static ANCESTOR_BLOOM_ENABLED: AtomicBool = AtomicBool::new(true);
static SELECTOR_CACHE_EPOCH: AtomicUsize = AtomicUsize::new(1);
static SELECTOR_BLOOM_SUMMARY_ENV_INITIALIZED: OnceLock<()> = OnceLock::new();
static SELECTOR_BLOOM_SUMMARY_BITS: AtomicUsize =
  const { AtomicUsize::new(SELECTOR_BLOOM_SUMMARY_BITS_DEFAULT) };

const SELECTOR_BLOOM_SUMMARY_BITS_DEFAULT: usize = 1024;

fn normalize_selector_bloom_summary_bits(bits: usize) -> usize {
  match bits {
    256 | 512 | 1024 => bits,
    _ => SELECTOR_BLOOM_SUMMARY_BITS_DEFAULT,
  }
}

fn selector_bloom_summary_bits() -> usize {
  SELECTOR_BLOOM_SUMMARY_ENV_INITIALIZED.get_or_init(|| {
    if let Ok(value) = std::env::var("FASTR_SELECTOR_BLOOM_BITS") {
      if let Ok(bits) = value.trim().parse::<usize>() {
        let bits = normalize_selector_bloom_summary_bits(bits);
        SELECTOR_BLOOM_SUMMARY_BITS.store(bits, Ordering::Relaxed);
      }
    }
  });
  SELECTOR_BLOOM_SUMMARY_BITS.load(Ordering::Relaxed)
}

/// Override the selector bloom summary size (in bits) for benchmarking/testing.
///
/// Supported values: 256, 512, 1024.
pub fn set_selector_bloom_summary_bits(bits: usize) {
  SELECTOR_BLOOM_SUMMARY_ENV_INITIALIZED.get_or_init(|| ());
  let bits = normalize_selector_bloom_summary_bits(bits);
  SELECTOR_BLOOM_SUMMARY_BITS.store(bits, Ordering::Relaxed);
}

pub(crate) fn selector_bloom_enabled() -> bool {
  SELECTOR_BLOOM_ENV_INITIALIZED.get_or_init(|| {
    if let Ok(value) = std::env::var("FASTR_SELECTOR_BLOOM") {
      if value.trim() == "0" {
        SELECTOR_BLOOM_ENABLED.store(false, Ordering::Relaxed);
      }
    }
  });
  SELECTOR_BLOOM_ENABLED.load(Ordering::Relaxed)
}

/// Toggle selector bloom-filter insertion for benchmarking/testing.
pub fn set_selector_bloom_enabled(enabled: bool) {
  SELECTOR_BLOOM_ENV_INITIALIZED.get_or_init(|| ());
  SELECTOR_BLOOM_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn ancestor_bloom_enabled() -> bool {
  ANCESTOR_BLOOM_ENV_INITIALIZED.get_or_init(|| {
    if let Ok(value) = std::env::var("FASTR_ANCESTOR_BLOOM") {
      if value.trim() == "0" {
        ANCESTOR_BLOOM_ENABLED.store(false, Ordering::Relaxed);
      }
    }
  });
  ANCESTOR_BLOOM_ENABLED.load(Ordering::Relaxed)
}

/// Toggle the cascade ancestor bloom filter for benchmarking/testing.
pub fn set_ancestor_bloom_enabled(enabled: bool) {
  ANCESTOR_BLOOM_ENV_INITIALIZED.get_or_init(|| ());
  ANCESTOR_BLOOM_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Returns a monotonically increasing epoch for selector caches.
pub fn next_selector_cache_epoch() -> usize {
  SELECTOR_CACHE_EPOCH.fetch_add(1, Ordering::Relaxed)
}

#[inline]
fn selector_bloom_hash(value: &str) -> u32 {
  crate::css::types::selector_hash(value) & selectors::bloom::BLOOM_HASH_MASK
}

fn node_is_html_element(node: &DomNode) -> bool {
  matches!(
    node.node_type,
    DomNodeType::Element { ref namespace, .. } | DomNodeType::Slot { ref namespace, .. }
      if namespace.is_empty() || namespace == HTML_NAMESPACE
  )
}

const SELECTOR_BLOOM_ASCII_LOWERCASE_STACK_BUF: usize = 64;

#[inline]
fn selector_bloom_hash_ascii_lowercase(value: &str) -> u32 {
  if value.bytes().any(|b| b.is_ascii_uppercase()) {
    selector_bloom_hash_ascii_lowercase_known_upper(value)
  } else {
    selector_bloom_hash(value)
  }
}

#[inline]
fn selector_bloom_hash_ascii_lowercase_known_upper(value: &str) -> u32 {
  let bytes = value.as_bytes();
  if bytes.len() <= SELECTOR_BLOOM_ASCII_LOWERCASE_STACK_BUF {
    let mut buf = [0u8; SELECTOR_BLOOM_ASCII_LOWERCASE_STACK_BUF];
    for (dst, &byte) in buf.iter_mut().zip(bytes.iter()) {
      *dst = byte.to_ascii_lowercase();
    }
    let lower = unsafe { std::str::from_utf8_unchecked(&buf[..bytes.len()]) };
    selector_bloom_hash(lower)
  } else {
    // Extremely long IDs/classes are rare; fall back to allocating.
    selector_bloom_hash(&value.to_ascii_lowercase())
  }
}

fn add_selector_bloom_hashes_internal(
  node: &DomNode,
  quirks_mode: QuirksMode,
  add: &mut impl FnMut(u32),
) {
  if !node.is_element() {
    return;
  }

  if let Some(namespace) = node.namespace() {
    add(selector_bloom_hash(namespace));
    // Treat missing namespaces as HTML for selector matching (see `ElementRef::has_namespace`).
    if namespace.is_empty() {
      add(selector_bloom_hash(HTML_NAMESPACE));
    }
  }

  let is_html = node_is_html_element(node);
  if let Some(tag) = node.tag_name() {
    if is_html {
      let has_upper = tag.bytes().any(|b| b.is_ascii_uppercase());
      if has_upper {
        add(selector_bloom_hash_ascii_lowercase_known_upper(tag));
      }
      // Most HTML tag names are already lowercase (html5ever lowercases), so avoid allocating.
      add(selector_bloom_hash(tag));
    } else {
      add(selector_bloom_hash(tag));
      if tag.bytes().any(|b| b.is_ascii_uppercase()) {
        add(selector_bloom_hash_ascii_lowercase_known_upper(tag));
      }
    }
  }

  let mut saw_id = false;
  let mut saw_class = false;
  for (name, value) in node.attributes_iter() {
    if !saw_id && name.eq_ignore_ascii_case("id") {
      if matches!(quirks_mode, QuirksMode::Quirks) {
        add(selector_bloom_hash_ascii_lowercase(value));
      } else {
        add(selector_bloom_hash(value));
      }
      saw_id = true;
    }
    if !saw_class && name.eq_ignore_ascii_case("class") {
      for class in value.split_ascii_whitespace() {
        if matches!(quirks_mode, QuirksMode::Quirks) {
          add(selector_bloom_hash_ascii_lowercase(class));
        } else {
          add(selector_bloom_hash(class));
        }
      }
      saw_class = true;
    }

    add(selector_bloom_hash(name));
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
      add(selector_bloom_hash_ascii_lowercase_known_upper(name));
    }
  }
}

fn add_selector_bloom_hashes(node: &DomNode, add: &mut impl FnMut(u32)) {
  add_selector_bloom_hashes_internal(node, QuirksMode::NoQuirks, add);
}

pub(crate) fn for_each_ancestor_bloom_hash(
  node: &DomNode,
  quirks_mode: QuirksMode,
  mut add: impl FnMut(u32),
) {
  if !node.is_element() {
    return;
  }

  let is_html = node_is_html_element(node);
  if let Some(tag) = node.tag_name() {
    let hash = if is_html {
      selector_bloom_hash_ascii_lowercase(tag)
    } else {
      selector_bloom_hash(tag)
    };
    add(hash);
  }

  let quirks_case_fold = matches!(quirks_mode, QuirksMode::Quirks) && is_html;
  let mut saw_id = false;
  let mut saw_class = false;
  for (name, value) in node.attributes_iter() {
    add(selector_bloom_hash_ascii_lowercase(name));

    if !saw_id && name.eq_ignore_ascii_case("id") {
      saw_id = true;
      if quirks_case_fold {
        if value.bytes().any(|b| b.is_ascii_uppercase()) {
          add(selector_bloom_hash_ascii_lowercase_known_upper(value));
          add(selector_bloom_hash(value));
        } else {
          add(selector_bloom_hash(value));
        }
      } else {
        add(selector_bloom_hash(value));
      }
      continue;
    }

    if !saw_class && name.eq_ignore_ascii_case("class") {
      saw_class = true;
      for class in value.split_ascii_whitespace() {
        if quirks_case_fold {
          if class.bytes().any(|b| b.is_ascii_uppercase()) {
            add(selector_bloom_hash_ascii_lowercase_known_upper(class));
            add(selector_bloom_hash(class));
          } else {
            add(selector_bloom_hash(class));
          }
        } else {
          add(selector_bloom_hash(class));
        }
      }
    }
  }
}
static HAS_EVALS: AtomicU64 = AtomicU64::new(0);
static HAS_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static HAS_PRUNES: AtomicU64 = AtomicU64::new(0);
static HAS_FILTER_PRUNES: AtomicU64 = AtomicU64::new(0);
static HAS_RELATIVE_EVALS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
  static HAS_COUNTERS: std::cell::Cell<HasCounters> = std::cell::Cell::new(HasCounters::default());
}

#[inline]
fn record_has_eval() {
  #[cfg(test)]
  HAS_COUNTERS.with(|c| {
    let mut counters = c.get();
    counters.evals += 1;
    c.set(counters);
  });
  #[cfg(not(test))]
  HAS_EVALS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_has_cache_hit() {
  #[cfg(test)]
  HAS_COUNTERS.with(|c| {
    let mut counters = c.get();
    counters.cache_hits += 1;
    c.set(counters);
  });
  #[cfg(not(test))]
  HAS_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_has_prune() {
  #[cfg(test)]
  HAS_COUNTERS.with(|c| {
    let mut counters = c.get();
    counters.prunes += 1;
    c.set(counters);
  });
  #[cfg(not(test))]
  HAS_PRUNES.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_has_filter_prune() {
  #[cfg(test)]
  HAS_COUNTERS.with(|c| {
    let mut counters = c.get();
    counters.filter_prunes += 1;
    c.set(counters);
  });
  #[cfg(not(test))]
  HAS_FILTER_PRUNES.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_has_relative_eval() {
  #[cfg(test)]
  HAS_COUNTERS.with(|c| {
    let mut counters = c.get();
    counters.evaluated += 1;
    c.set(counters);
  });
  #[cfg(not(test))]
  HAS_RELATIVE_EVALS.fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HasCounters {
  pub evals: u64,
  pub cache_hits: u64,
  /// Bloom-summary based prunes plus fast-reject bloom filters.
  pub prunes: u64,
  /// Prunes coming from the subtree bloom filters built on demand.
  pub filter_prunes: u64,
  /// Relative selector evaluations that were executed after pruning and caching.
  pub evaluated: u64,
}

impl HasCounters {
  pub fn summary_prunes(&self) -> u64 {
    self.prunes.saturating_sub(self.filter_prunes)
  }
}

pub fn reset_has_counters() {
  #[cfg(test)]
  HAS_COUNTERS.with(|c| c.set(HasCounters::default()));
  #[cfg(not(test))]
  {
    HAS_EVALS.store(0, Ordering::Relaxed);
    HAS_CACHE_HITS.store(0, Ordering::Relaxed);
    HAS_PRUNES.store(0, Ordering::Relaxed);
    HAS_FILTER_PRUNES.store(0, Ordering::Relaxed);
    HAS_RELATIVE_EVALS.store(0, Ordering::Relaxed);
  }
}

pub fn capture_has_counters() -> HasCounters {
  #[cfg(test)]
  {
    HAS_COUNTERS.with(|c| c.get())
  }
  #[cfg(not(test))]
  {
    HasCounters {
      evals: HAS_EVALS.load(Ordering::Relaxed),
      cache_hits: HAS_CACHE_HITS.load(Ordering::Relaxed),
      prunes: HAS_PRUNES.load(Ordering::Relaxed),
      filter_prunes: HAS_FILTER_PRUNES.load(Ordering::Relaxed),
      evaluated: HAS_RELATIVE_EVALS.load(Ordering::Relaxed),
    }
  }
}

fn insert_summary_hash<const WORDS: usize>(summary: &mut [u64; WORDS], hash: u32) {
  let bits = WORDS * 64;
  debug_assert!(bits.is_power_of_two());
  let mask = bits - 1;
  let shift = bits.trailing_zeros() as usize;
  let slot_a = (hash as usize) & mask;
  let slot_b = ((hash as usize) >> shift) & mask;
  insert_summary_slot(summary, slot_a);
  insert_summary_slot(summary, slot_b);
}

fn insert_summary_slot<const WORDS: usize>(summary: &mut [u64; WORDS], slot: usize) {
  let (idx, bit) = (slot / 64, slot % 64);
  summary[idx] |= 1u64 << bit;
}

fn summary_contains_hash<const WORDS: usize>(summary: &[u64; WORDS], hash: u32) -> bool {
  let bits = WORDS * 64;
  debug_assert!(bits.is_power_of_two());
  let mask = bits - 1;
  let shift = bits.trailing_zeros() as usize;
  let slot_a = (hash as usize) & mask;
  let slot_b = ((hash as usize) >> shift) & mask;
  summary_contains_slot(summary, slot_a) && summary_contains_slot(summary, slot_b)
}

fn summary_contains_slot<const WORDS: usize>(summary: &[u64; WORDS], slot: usize) -> bool {
  let (idx, bit) = (slot / 64, slot % 64);
  (summary[idx] & (1u64 << bit)) != 0
}

fn merge_summary<const WORDS: usize>(summary: &mut [u64; WORDS], other: &[u64; WORDS]) {
  for (dst, src) in summary.iter_mut().zip(other.iter()) {
    *dst |= *src;
  }
}

#[derive(Debug, Clone, Copy)]
pub enum SelectorBloomSummaryRef<'a> {
  Bits256(&'a [u64; 4]),
  Bits512(&'a [u64; 8]),
  Bits1024(&'a [u64; 16]),
}

impl<'a> SelectorBloomSummaryRef<'a> {
  pub(crate) fn contains_hash(&self, hash: u32) -> bool {
    match self {
      Self::Bits256(summary) => summary_contains_hash(summary, hash),
      Self::Bits512(summary) => summary_contains_hash(summary, hash),
      Self::Bits1024(summary) => summary_contains_hash(summary, hash),
    }
  }

  pub fn words(&self) -> &'a [u64] {
    match *self {
      Self::Bits256(summary) => summary.as_ref(),
      Self::Bits512(summary) => summary.as_ref(),
      Self::Bits1024(summary) => summary.as_ref(),
    }
  }
}

/// A dense, node-id indexed store of selector bloom summaries.
///
/// Contract: `node_id` comes from [`enumerate_dom_ids`] and starts at 1. Index 0 is unused.
#[derive(Debug, Clone)]
pub enum SelectorBloomStore {
  Bits256(SelectorBloomStoreImpl<4>),
  Bits512(SelectorBloomStoreImpl<8>),
  Bits1024(SelectorBloomStoreImpl<16>),
}

#[derive(Debug, Clone)]
pub struct SelectorBloomStoreImpl<const WORDS: usize> {
  summaries: Vec<[u64; WORDS]>,
}

impl SelectorBloomStore {
  pub fn summary_for_id(&self, node_id: usize) -> Option<SelectorBloomSummaryRef<'_>> {
    if node_id == 0 {
      return None;
    }
    match self {
      Self::Bits256(store) => store
        .summaries
        .get(node_id)
        .map(SelectorBloomSummaryRef::Bits256),
      Self::Bits512(store) => store
        .summaries
        .get(node_id)
        .map(SelectorBloomSummaryRef::Bits512),
      Self::Bits1024(store) => store
        .summaries
        .get(node_id)
        .map(SelectorBloomSummaryRef::Bits1024),
    }
  }
}

/// Build selector bloom summaries for each element node, indexed by `node_id`.
pub fn build_selector_bloom_store(
  root: &DomNode,
  id_map: &HashMap<*const DomNode, usize>,
) -> Option<SelectorBloomStore> {
  if !selector_bloom_enabled() {
    return None;
  }

  let quirks_mode = match &root.node_type {
    DomNodeType::Document { quirks_mode } => *quirks_mode,
    _ => QuirksMode::NoQuirks,
  };
  let raw_bits = selector_bloom_summary_bits();
  let bits = normalize_selector_bloom_summary_bits(raw_bits);
  debug_assert_eq!(
    raw_bits, bits,
    "selector bloom summary bits should be normalised: {raw_bits}"
  );
  match bits {
    256 => build_selector_bloom_store_impl::<4>(root, id_map, quirks_mode)
      .map(SelectorBloomStore::Bits256),
    512 => build_selector_bloom_store_impl::<8>(root, id_map, quirks_mode)
      .map(SelectorBloomStore::Bits512),
    _ => build_selector_bloom_store_impl::<16>(root, id_map, quirks_mode)
      .map(SelectorBloomStore::Bits1024),
  }
}

fn build_selector_bloom_store_impl<const WORDS: usize>(
  root: &DomNode,
  id_map: &HashMap<*const DomNode, usize>,
  quirks_mode: QuirksMode,
) -> Option<SelectorBloomStoreImpl<WORDS>> {
  fn try_push<T>(vec: &mut Vec<T>, value: T) -> Option<()> {
    if vec.len() == vec.capacity() {
      // Grow in bounded exponential steps so we don't over-allocate aggressively, while still
      // avoiding O(n^2) realloc behaviour for deep DOM stacks.
      let additional = vec.capacity().max(1);
      vec.try_reserve(additional).ok()?;
    }
    vec.push(value);
    Some(())
  }

  // Keep index 0 unused so the 1-based `node_id` from `enumerate_dom_ids` can be used directly.
  //
  // We still accept `id_map` to reserve the right size up-front, but we avoid doing a pointer-keyed
  // HashMap lookup per element by assigning ids during a pre-order traversal (the same order as
  // `enumerate_dom_ids`).
  let mut summaries: Vec<[u64; WORDS]> = Vec::new();
  if summaries
    .try_reserve_exact(id_map.len().saturating_add(1))
    .is_err()
  {
    return None;
  }
  summaries.push([0u64; WORDS]);

  struct Frame<'a, const WORDS: usize> {
    node: &'a DomNode,
    id: usize,
    is_element: bool,
    is_shadow_root: bool,
    is_template: bool,
    next_child: usize,
    summary: [u64; WORDS],
  }

  let mut stack: Vec<Frame<'_, WORDS>> = Vec::new();
  if stack.try_reserve_exact(id_map.len().min(1024)).is_err() {
    return None;
  }

  let root_id = summaries.len();
  try_push(&mut summaries, [0u64; WORDS])?;
  let root_is_element = root.is_element();
  let root_is_template = root.template_contents_are_inert();
  let mut root_summary = [0u64; WORDS];
  if root_is_element {
    add_selector_bloom_hashes_internal(root, quirks_mode, &mut |hash| {
      insert_summary_hash(&mut root_summary, hash);
    });
  }
  try_push(
    &mut stack,
    Frame {
      node: root,
      id: root_id,
      is_element: root_is_element,
      is_shadow_root: matches!(root.node_type, DomNodeType::ShadowRoot { .. }),
      is_template: root_is_template,
      next_child: 0,
      summary: root_summary,
    },
  )?;

  while let Some(mut frame) = stack.pop() {
    // `enumerate_dom_ids` includes template contents as DOM nodes even though they are inert for
    // selector matching. Still traverse `children` here so the bloom store stays aligned with
    // `node_id`, but block summary merging when the parent is a `<template>` so ancestors never see
    // the inert template subtree.
    let children = frame.node.children.as_slice();
    if frame.next_child < children.len() {
      let child = &children[frame.next_child];
      frame.next_child += 1;
      try_push(&mut stack, frame)?;

      let child_id = summaries.len();
      try_push(&mut summaries, [0u64; WORDS])?;

      let child_is_element = child.is_element();
      let child_is_template = child.template_contents_are_inert();
      let mut child_summary = [0u64; WORDS];
      if child_is_element {
        add_selector_bloom_hashes_internal(child, quirks_mode, &mut |hash| {
          insert_summary_hash(&mut child_summary, hash);
        });
      }

      try_push(
        &mut stack,
        Frame {
          node: child,
          id: child_id,
          is_element: child_is_element,
          is_shadow_root: matches!(child.node_type, DomNodeType::ShadowRoot { .. }),
          is_template: child_is_template,
          next_child: 0,
          summary: child_summary,
        },
      )?;
      continue;
    }

    if frame.is_element {
      summaries[frame.id] = frame.summary;
    }

    if let Some(parent) = stack.last_mut() {
      if parent.is_element && !parent.is_template && !frame.is_shadow_root {
        merge_summary(&mut parent.summary, &frame.summary);
      }
    }
  }
  debug_assert_eq!(
    summaries.len(),
    id_map.len().saturating_add(1),
    "selector bloom store should align with enumerate_dom_ids node ids"
  );
  Some(SelectorBloomStoreImpl { summaries })
}

#[cfg(test)]
type SelectorBloomMapLegacy<const WORDS: usize> = HashMap<*const DomNode, [u64; WORDS]>;

#[cfg(test)]
fn build_selector_bloom_map_legacy<const WORDS: usize>(
  root: &DomNode,
) -> Option<SelectorBloomMapLegacy<WORDS>> {
  if !selector_bloom_enabled() {
    return None;
  }

  fn walk<const WORDS: usize>(
    node: &DomNode,
    map: &mut SelectorBloomMapLegacy<WORDS>,
  ) -> [u64; WORDS] {
    let mut summary = [0u64; WORDS];
    if node.is_element() {
      add_selector_bloom_hashes(node, &mut |hash| insert_summary_hash(&mut summary, hash));
    }

    for child in node.children.iter() {
      let child_summary = if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
        walk(child, map);
        None
      } else {
        Some(walk(child, map))
      };
      if node.is_element() && !node.template_contents_are_inert() {
        if let Some(summary_child) = child_summary.as_ref() {
          merge_summary(&mut summary, summary_child);
        }
      }
    }

    if node.is_element() {
      map.insert(node as *const DomNode, summary);
    }

    summary
  }

  let mut blooms: SelectorBloomMapLegacy<WORDS> = SelectorBloomMapLegacy::new();
  walk(root, &mut blooms);
  Some(blooms)
}

#[derive(Clone, Copy, Debug)]
pub struct SiblingPosition {
  pub index: usize,
  pub len: usize,
  pub type_index: usize,
  pub type_len: usize,
}

#[derive(Debug, Default)]
pub struct SiblingListCache {
  _epoch: usize,
  parents: RefCell<HashMap<*const DomNode, ParentSiblingList>>,
}

#[derive(Debug, Default)]
struct ParentSiblingList {
  positions: HashMap<*const DomNode, SiblingPosition>,
  elements: Vec<*const DomNode>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SiblingTypeKey {
  namespace: String,
  local_name: String,
}

impl SiblingListCache {
  pub fn new(epoch: usize) -> Self {
    Self {
      _epoch: epoch,
      parents: RefCell::new(HashMap::new()),
    }
  }

  pub fn position(
    &self,
    parent: &DomNode,
    child: &DomNode,
    context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
  ) -> Option<SiblingPosition> {
    let parent_ptr = parent as *const DomNode;
    {
      let parents = self.parents.borrow();
      if let Some(entry) = parents.get(&parent_ptr) {
        if let Some(position) = entry.positions.get(&(child as *const DomNode)) {
          return Some(*position);
        }
      }
    }

    let entry = build_parent_sibling_list(parent, context)?;
    let mut parents = self.parents.borrow_mut();
    let cached = parents.entry(parent_ptr).or_insert(entry);
    cached.positions.get(&(child as *const DomNode)).copied()
  }

  pub fn ordered_children(
    &self,
    parent: &DomNode,
    context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
  ) -> Option<Vec<*const DomNode>> {
    let parent_ptr = parent as *const DomNode;
    {
      let parents = self.parents.borrow();
      if let Some(entry) = parents.get(&parent_ptr) {
        return Some(entry.elements.clone());
      }
    }

    let entry = build_parent_sibling_list(parent, context)?;
    let elements = entry.elements.clone();
    self.parents.borrow_mut().insert(parent_ptr, entry);
    Some(elements)
  }
}

const ELEMENT_ATTR_CACHE_ATTR_INDEX_THRESHOLD: usize = 10;
const ELEMENT_ATTR_CACHE_CLASS_INDEX_THRESHOLD: usize = 8;
const ELEMENT_ATTR_CACHE_FNV_OFFSET_BASIS: u64 = 14695981039346656037;
const ELEMENT_ATTR_CACHE_FNV_PRIME: u64 = 1099511628211;

#[derive(Default)]
struct ElementAttrCacheHasher(u64);

impl Hasher for ElementAttrCacheHasher {
  fn write(&mut self, bytes: &[u8]) {
    let mut hash = ELEMENT_ATTR_CACHE_FNV_OFFSET_BASIS;
    for &byte in bytes {
      hash ^= byte as u64;
      hash = hash.wrapping_mul(ELEMENT_ATTR_CACHE_FNV_PRIME);
    }
    self.0 = hash;
  }

  fn write_u64(&mut self, i: u64) {
    self.0 = i;
  }

  fn write_usize(&mut self, i: usize) {
    self.0 = i as u64;
  }

  fn finish(&self) -> u64 {
    self.0
  }
}

type ElementAttrCacheBuildHasher = BuildHasherDefault<ElementAttrCacheHasher>;

#[inline]
fn element_attr_cache_hash_ascii_lowercase(value: &str) -> u64 {
  let mut hash = ELEMENT_ATTR_CACHE_FNV_OFFSET_BASIS;
  for byte in value.bytes() {
    let folded = if byte >= b'A' && byte <= b'Z' {
      byte + 32
    } else {
      byte
    };
    hash ^= folded as u64;
    hash = hash.wrapping_mul(ELEMENT_ATTR_CACHE_FNV_PRIME);
  }
  hash
}

#[inline]
fn element_attr_cache_hash_str(value: &str) -> u64 {
  let mut hash = ELEMENT_ATTR_CACHE_FNV_OFFSET_BASIS;
  for &byte in value.as_bytes() {
    hash ^= byte as u64;
    hash = hash.wrapping_mul(ELEMENT_ATTR_CACHE_FNV_PRIME);
  }
  hash
}

#[inline]
fn element_attr_cache_name_hash(name: &str, is_html: bool) -> u64 {
  if is_html {
    element_attr_cache_hash_ascii_lowercase(name)
  } else {
    element_attr_cache_hash_str(name)
  }
}

#[inline]
fn element_attr_cache_name_matches(actual: &str, expected: &str, is_html: bool) -> bool {
  if is_html {
    actual.eq_ignore_ascii_case(expected)
  } else {
    actual == expected
  }
}

#[derive(Debug, Clone)]
enum CachedClassTokens {
  None,
  Unparsed(*const str),
  Parsed {
    raw: *const str,
    ranges: Box<[std::ops::Range<usize>]>,
    index_sensitive: CachedClassIndex,
    index_ascii: CachedClassIndex,
  },
}

#[derive(Debug, Clone)]
enum CachedClassIndex {
  Pending,
  Disabled,
  Built(HashMap<u64, ClassBucket, ElementAttrCacheBuildHasher>),
}

#[derive(Debug, Clone)]
enum ClassBucket {
  Single(usize),
  Multi(Vec<usize>),
}

#[derive(Debug, Clone)]
enum CachedAttrIndex {
  Pending,
  Disabled,
  Built(HashMap<u64, AttrBucket, ElementAttrCacheBuildHasher>),
}

#[derive(Debug, Clone)]
enum AttrBucket {
  Single(usize),
  Multi(Vec<usize>),
}

#[derive(Debug, Clone)]
struct ElementAttrCacheEntry {
  is_html: bool,
  id: Option<*const str>,
  class: CachedClassTokens,
  attr_index: CachedAttrIndex,
  ancestor_bloom_hashes: Option<Box<[u32]>>,
  selector_bloom_hashes: Option<Box<[u32]>>,
}

impl ElementAttrCacheEntry {
  fn new(node: &DomNode) -> Self {
    let is_html = node_is_html_element(node);
    let attrs: &[(String, String)] = match &node.node_type {
      DomNodeType::Element { attributes, .. } => attributes,
      DomNodeType::Slot { attributes, .. } => attributes,
      _ => &[],
    };

    let mut id: Option<*const str> = None;
    let mut class: Option<*const str> = None;
    for (name, value) in attrs.iter() {
      if id.is_none() && element_attr_cache_name_matches(name, "id", is_html) {
        id = Some(value.as_str() as *const str);
      }
      if class.is_none() && element_attr_cache_name_matches(name, "class", is_html) {
        class = Some(value.as_str() as *const str);
      }
      if id.is_some() && class.is_some() {
        break;
      }
    }

    let class = match class {
      Some(raw) => CachedClassTokens::Unparsed(raw),
      None => CachedClassTokens::None,
    };

    Self {
      is_html,
      id,
      class,
      attr_index: CachedAttrIndex::Pending,
      ancestor_bloom_hashes: None,
      selector_bloom_hashes: None,
    }
  }

  fn ensure_class_parsed(&mut self) {
    let raw_ptr = match &self.class {
      CachedClassTokens::Unparsed(ptr) => Some(*ptr),
      _ => None,
    };

    let Some(ptr) = raw_ptr else {
      return;
    };

    let raw: &str = unsafe { &*ptr };
    let base_ptr = raw.as_ptr() as usize;
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for token in raw.split_ascii_whitespace() {
      let start = token.as_ptr() as usize - base_ptr;
      ranges.push(start..start + token.len());
    }
    let ranges = ranges.into_boxed_slice();
    let index_state = if ranges.len() >= ELEMENT_ATTR_CACHE_CLASS_INDEX_THRESHOLD {
      CachedClassIndex::Pending
    } else {
      CachedClassIndex::Disabled
    };

    self.class = CachedClassTokens::Parsed {
      raw: ptr,
      ranges,
      index_sensitive: index_state.clone(),
      index_ascii: index_state,
    };
  }

  fn class_index<'a>(
    index: &'a mut CachedClassIndex,
    raw: &str,
    ranges: &[std::ops::Range<usize>],
    case_sensitivity: CaseSensitivity,
  ) -> Option<&'a HashMap<u64, ClassBucket, ElementAttrCacheBuildHasher>> {
    if matches!(index, CachedClassIndex::Pending) {
      let mut map: HashMap<u64, ClassBucket, ElementAttrCacheBuildHasher> = HashMap::default();
      for (idx, range) in ranges.iter().enumerate() {
        let token = &raw[range.start..range.end];
        if token.is_empty() {
          continue;
        }
        let hash = match case_sensitivity {
          CaseSensitivity::CaseSensitive => element_attr_cache_hash_str(token),
          CaseSensitivity::AsciiCaseInsensitive => element_attr_cache_hash_ascii_lowercase(token),
        };
        match map.entry(hash) {
          std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(ClassBucket::Single(idx));
          }
          std::collections::hash_map::Entry::Occupied(mut entry) => {
            let bucket = entry.get_mut();
            let is_dup = match bucket {
              ClassBucket::Single(existing) => {
                let existing_range = ranges.get(*existing);
                existing_range.is_some_and(|r| match case_sensitivity {
                  CaseSensitivity::CaseSensitive => &raw[r.start..r.end] == token,
                  CaseSensitivity::AsciiCaseInsensitive => {
                    raw[r.start..r.end].eq_ignore_ascii_case(token)
                  }
                })
              }
              ClassBucket::Multi(existing) => existing.iter().any(|existing| {
                let Some(r) = ranges.get(*existing) else {
                  return false;
                };
                match case_sensitivity {
                  CaseSensitivity::CaseSensitive => &raw[r.start..r.end] == token,
                  CaseSensitivity::AsciiCaseInsensitive => {
                    raw[r.start..r.end].eq_ignore_ascii_case(token)
                  }
                }
              }),
            };
            if is_dup {
              continue;
            }
            match bucket {
              ClassBucket::Single(existing) => {
                let prev = *existing;
                *bucket = ClassBucket::Multi(vec![prev, idx]);
              }
              ClassBucket::Multi(existing) => existing.push(idx),
            }
          }
        }
      }
      *index = CachedClassIndex::Built(map);
    }

    match index {
      CachedClassIndex::Built(map) => Some(map),
      _ => None,
    }
  }

  fn has_class(&mut self, class: &str, case_sensitivity: CaseSensitivity) -> bool {
    self.ensure_class_parsed();
    let CachedClassTokens::Parsed {
      raw,
      ranges,
      index_sensitive,
      index_ascii,
    } = &mut self.class
    else {
      return false;
    };

    let raw: &str = unsafe { &**raw };
    let ranges = ranges.as_ref();

    let index = match case_sensitivity {
      CaseSensitivity::CaseSensitive => {
        Self::class_index(index_sensitive, raw, ranges, CaseSensitivity::CaseSensitive)
      }
      CaseSensitivity::AsciiCaseInsensitive => Self::class_index(
        index_ascii,
        raw,
        ranges,
        CaseSensitivity::AsciiCaseInsensitive,
      ),
    };

    if let Some(index) = index {
      let query_hash = match case_sensitivity {
        CaseSensitivity::CaseSensitive => element_attr_cache_hash_str(class),
        CaseSensitivity::AsciiCaseInsensitive => element_attr_cache_hash_ascii_lowercase(class),
      };
      if let Some(bucket) = index.get(&query_hash) {
        let indices: &[usize] = match bucket {
          ClassBucket::Single(idx) => std::slice::from_ref(idx),
          ClassBucket::Multi(list) => list.as_slice(),
        };
        for idx in indices {
          let Some(range) = ranges.get(*idx) else {
            continue;
          };
          let token = &raw[range.start..range.end];
          let matches = match case_sensitivity {
            CaseSensitivity::CaseSensitive => token == class,
            CaseSensitivity::AsciiCaseInsensitive => token.eq_ignore_ascii_case(class),
          };
          if matches {
            return true;
          }
        }
        return false;
      }
    }

    match case_sensitivity {
      CaseSensitivity::CaseSensitive => ranges
        .iter()
        .any(|range| &raw[range.start..range.end] == class),
      CaseSensitivity::AsciiCaseInsensitive => ranges
        .iter()
        .any(|range| raw[range.start..range.end].eq_ignore_ascii_case(class)),
    }
  }

  fn attr_index<'a>(
    &'a mut self,
    node: &DomNode,
  ) -> Option<(
    &'a HashMap<u64, AttrBucket, ElementAttrCacheBuildHasher>,
    bool,
  )> {
    if matches!(self.attr_index, CachedAttrIndex::Pending) {
      let attrs: &[(String, String)] = match &node.node_type {
        DomNodeType::Element { attributes, .. } => attributes,
        DomNodeType::Slot { attributes, .. } => attributes,
        _ => &[],
      };
      if attrs.len() < ELEMENT_ATTR_CACHE_ATTR_INDEX_THRESHOLD {
        self.attr_index = CachedAttrIndex::Disabled;
      } else {
        let mut map: HashMap<u64, AttrBucket, ElementAttrCacheBuildHasher> = HashMap::default();
        for (idx, (name, _)) in attrs.iter().enumerate() {
          let hash = element_attr_cache_name_hash(name, self.is_html);
          match map.entry(hash) {
            std::collections::hash_map::Entry::Vacant(entry) => {
              entry.insert(AttrBucket::Single(idx));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
              let bucket = entry.get_mut();
              let is_dup = match bucket {
                AttrBucket::Single(existing) => {
                  let existing_name = attrs.get(*existing).map(|(n, _)| n.as_str()).unwrap_or("");
                  element_attr_cache_name_matches(existing_name, name, self.is_html)
                }
                AttrBucket::Multi(existing) => existing.iter().any(|existing| {
                  let existing_name = attrs.get(*existing).map(|(n, _)| n.as_str()).unwrap_or("");
                  element_attr_cache_name_matches(existing_name, name, self.is_html)
                }),
              };
              if is_dup {
                continue;
              }
              match bucket {
                AttrBucket::Single(existing) => {
                  let prev = *existing;
                  *bucket = AttrBucket::Multi(vec![prev, idx]);
                }
                AttrBucket::Multi(existing) => existing.push(idx),
              }
            }
          }
        }
        self.attr_index = CachedAttrIndex::Built(map);
      }
    }

    match &self.attr_index {
      CachedAttrIndex::Built(map) => Some((map, self.is_html)),
      _ => None,
    }
  }
}

#[derive(Debug)]
pub struct ElementAttrCache {
  _epoch: usize,
  entries: RefCell<HashMap<*const DomNode, ElementAttrCacheEntry, ElementAttrCacheBuildHasher>>,
}

impl ElementAttrCache {
  pub fn new(epoch: usize) -> Self {
    Self {
      _epoch: epoch,
      entries: RefCell::new(HashMap::default()),
    }
  }

  pub fn clear(&self) {
    self.entries.borrow_mut().clear();
  }

  fn entry_mut<'a>(&'a self, node: &DomNode) -> RefMut<'a, ElementAttrCacheEntry> {
    let ptr = node as *const DomNode;
    RefMut::map(self.entries.borrow_mut(), |entries| {
      entries
        .entry(ptr)
        .or_insert_with(|| ElementAttrCacheEntry::new(node))
    })
  }

  pub fn for_each_ancestor_bloom_hash(
    &self,
    node: &DomNode,
    quirks_mode: QuirksMode,
    mut add: impl FnMut(u32),
  ) {
    let mut entry = self.entry_mut(node);
    if entry.ancestor_bloom_hashes.is_none() {
      let mut hashes = Vec::new();
      for_each_ancestor_bloom_hash(node, quirks_mode, |hash| hashes.push(hash));
      entry.ancestor_bloom_hashes = Some(hashes.into_boxed_slice());
    }
    if let Some(hashes) = entry.ancestor_bloom_hashes.as_deref() {
      for &hash in hashes {
        add(hash);
      }
    }
  }

  pub fn for_each_selector_bloom_hash(&self, node: &DomNode, mut add: impl FnMut(u32)) {
    let mut entry = self.entry_mut(node);
    if entry.selector_bloom_hashes.is_none() {
      let mut hashes = Vec::new();
      add_selector_bloom_hashes(node, &mut |hash| hashes.push(hash));
      entry.selector_bloom_hashes = Some(hashes.into_boxed_slice());
    }
    if let Some(hashes) = entry.selector_bloom_hashes.as_deref() {
      for &hash in hashes {
        add(hash);
      }
    }
  }

  pub fn has_id(&self, node: &DomNode, id: &str, case_sensitivity: CaseSensitivity) -> bool {
    let entry = self.entry_mut(node);
    let Some(id_ptr) = entry.id else {
      return false;
    };
    let actual: &str = unsafe { &*id_ptr };
    match case_sensitivity {
      CaseSensitivity::CaseSensitive => actual == id,
      CaseSensitivity::AsciiCaseInsensitive => actual.eq_ignore_ascii_case(id),
    }
  }

  pub fn has_class(&self, node: &DomNode, class: &str, case_sensitivity: CaseSensitivity) -> bool {
    self.entry_mut(node).has_class(class, case_sensitivity)
  }

  pub fn attr_value<'a>(&self, node: &'a DomNode, name: &str) -> Option<&'a str> {
    let mut entry = self.entry_mut(node);
    let attrs: &'a [(String, String)] = match &node.node_type {
      DomNodeType::Element { attributes, .. } => attributes,
      DomNodeType::Slot { attributes, .. } => attributes,
      _ => return None,
    };

    let query_hash = element_attr_cache_name_hash(name, entry.is_html);
    if let Some((index, is_html)) = entry.attr_index(node) {
      if let Some(bucket) = index.get(&query_hash) {
        let indices: &[usize] = match bucket {
          AttrBucket::Single(idx) => std::slice::from_ref(idx),
          AttrBucket::Multi(list) => list.as_slice(),
        };
        for idx in indices {
          let (attr_name, attr_value) = attrs.get(*idx)?;
          if element_attr_cache_name_matches(attr_name, name, is_html) {
            return Some(attr_value.as_str());
          }
        }
        return None;
      }
    }

    for (attr_name, attr_value) in attrs.iter() {
      if element_attr_cache_name_matches(attr_name, name, entry.is_html) {
        return Some(attr_value.as_str());
      }
    }

    None
  }
}

fn sibling_type_key(node: &DomNode) -> Option<SiblingTypeKey> {
  let tag = node.tag_name()?;
  let namespace = node.namespace().unwrap_or("").to_string();
  let is_html = node_is_html_element(node);
  let local_name = if is_html {
    tag.to_ascii_lowercase()
  } else {
    tag.to_string()
  };
  Some(SiblingTypeKey {
    namespace,
    local_name,
  })
}

fn build_parent_sibling_list(
  parent: &DomNode,
  context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
) -> Option<ParentSiblingList> {
  let mut deadline_counter = 0usize;
  let mut elements: Vec<(*const DomNode, SiblingTypeKey)> = Vec::new();
  for child in parent.children.iter() {
    if let Err(err) = check_active_periodic(
      &mut deadline_counter,
      NTH_DEADLINE_STRIDE,
      RenderStage::Cascade,
    ) {
      context.extra_data.record_deadline_error(err);
      return None;
    }
    if !child.is_element() {
      continue;
    }
    let Some(key) = sibling_type_key(child) else {
      continue;
    };
    elements.push((child as *const DomNode, key));
  }

  let len = elements.len();
  let mut type_totals: HashMap<SiblingTypeKey, usize> = HashMap::new();
  for (_, key) in elements.iter() {
    *type_totals.entry(key.clone()).or_insert(0) += 1;
  }
  let mut type_seen: HashMap<SiblingTypeKey, usize> = HashMap::new();
  let mut positions: HashMap<*const DomNode, SiblingPosition> = HashMap::with_capacity(len);
  for (idx, (ptr, key)) in elements.iter().enumerate() {
    let count = type_seen.entry(key.clone()).or_insert(0);
    let type_index = *count;
    *count += 1;
    let type_len = type_totals.get(key).copied().unwrap_or(0);
    positions.insert(
      *ptr,
      SiblingPosition {
        index: idx,
        len,
        type_index,
        type_len,
      },
    );
  }

  Some(ParentSiblingList {
    positions,
    elements: elements.iter().map(|(ptr, _)| *ptr).collect(),
  })
}

/// Resolve the first-strong direction within this subtree, skipping script/style contents.
pub fn resolve_first_strong_direction(node: &DomNode) -> Option<TextDirection> {
  let mut stack = vec![node];
  while let Some(current) = stack.pop() {
    match &current.node_type {
      DomNodeType::Text { content } => {
        for ch in content.chars() {
          match bidi_class(ch) {
            unicode_bidi::BidiClass::L => return Some(TextDirection::Ltr),
            unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL => {
              return Some(TextDirection::Rtl)
            }
            _ => {}
          }
        }
      }
      DomNodeType::Element { tag_name, .. } => {
        let skip = tag_name.eq_ignore_ascii_case("script")
          || tag_name.eq_ignore_ascii_case("style")
          || tag_name.eq_ignore_ascii_case("template");
        if skip {
          continue;
        }
        for child in &current.children {
          stack.push(child);
        }
      }
      DomNodeType::Slot { .. } => {
        for child in &current.children {
          stack.push(child);
        }
      }
      DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. } => {
        for child in &current.children {
          stack.push(child);
        }
      }
    }
  }
  None
}

fn node_is_hidden(attributes: &[(String, String)]) -> bool {
  attributes.iter().any(|(name, value)| {
    if name.eq_ignore_ascii_case("hidden") {
      return true;
    }
    if name.eq_ignore_ascii_case("data-fastr-hidden") {
      return value.eq_ignore_ascii_case("true");
    }
    false
  })
}

/// Collects the set of Unicode codepoints present in text nodes.
///
/// Script and style contents are skipped to avoid counting non-visible text. Nodes marked as
/// hidden (`[hidden]` / `data-fastr-hidden=true`) are ignored along with their descendants.
pub fn collect_text_codepoints(node: &DomNode) -> Result<Vec<u32>> {
  const CODEPOINT_DEADLINE_STRIDE: usize = 1024;
  let mut stack = vec![(node, false)];
  let mut seen: FxHashSet<u32> = FxHashSet::default();
  seen.reserve(256);
  let mut deadline_counter = 0usize;

  while let Some((current, suppressed)) = stack.pop() {
    check_active_periodic(
      &mut deadline_counter,
      CODEPOINT_DEADLINE_STRIDE,
      RenderStage::Css,
    )
    .map_err(Error::Render)?;
    if suppressed {
      continue;
    }
    match &current.node_type {
      DomNodeType::Text { content } => {
        if content.is_ascii() {
          for &b in content.as_bytes() {
            seen.insert(b as u32);
          }
        } else {
          for ch in content.chars() {
            seen.insert(ch as u32);
          }
        }
      }
      DomNodeType::Element {
        tag_name,
        attributes,
        ..
      } => {
        let skip = tag_name.eq_ignore_ascii_case("script")
          || tag_name.eq_ignore_ascii_case("style")
          || tag_name.eq_ignore_ascii_case("template");
        if skip {
          continue;
        }
        let suppress_children = node_is_hidden(attributes);
        for child in &current.children {
          stack.push((child, suppress_children));
        }
      }
      DomNodeType::Slot { attributes, .. } => {
        let suppress_children = node_is_hidden(attributes);
        for child in &current.children {
          stack.push((child, suppress_children));
        }
      }
      DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. } => {
        for child in &current.children {
          stack.push((child, false));
        }
      }
    }
  }

  let mut codepoints: Vec<u32> = seen.into_iter().collect();
  codepoints.sort_unstable();
  Ok(codepoints)
}

fn boolish(value: &str) -> bool {
  value == "1"
    || value.eq_ignore_ascii_case("true")
    || value.eq_ignore_ascii_case("yes")
    || value.eq_ignore_ascii_case("on")
    || value.eq_ignore_ascii_case("open")
}

fn data_fastr_open_state(node: &DomNode) -> Option<(bool, bool)> {
  let value = node.get_attribute_ref("data-fastr-open")?;
  if value.eq_ignore_ascii_case("false") {
    return Some((false, false));
  }
  if value.eq_ignore_ascii_case("modal") {
    return Some((true, true));
  }
  if boolish(value) {
    return Some((true, false));
  }
  None
}

fn dialog_state(node: &DomNode) -> Option<(bool, bool)> {
  if !node
    .tag_name()
    .map(|t| t.eq_ignore_ascii_case("dialog"))
    .unwrap_or(false)
  {
    return None;
  }

  let mut open = node.get_attribute_ref("open").is_some();
  let mut modal = node
    .get_attribute_ref("data-fastr-modal")
    .map(boolish)
    .unwrap_or(false);
  if let Some((open_override, modal_override)) = data_fastr_open_state(node) {
    open = open_override;
    modal |= modal_override;
  }

  if !open {
    return None;
  }

  Some((open, modal))
}

fn popover_open_assuming_popover(node: &DomNode) -> bool {
  let mut open = node.get_attribute_ref("open").is_some();
  if let Some((open_override, _)) = data_fastr_open_state(node) {
    open = open_override;
  }
  open
}

fn popover_open(node: &DomNode) -> bool {
  node.get_attribute_ref("popover").is_some() && popover_open_assuming_popover(node)
}

/// Bench helper: determine whether the DOM contains an open modal `<dialog>`.
#[doc(hidden)]
pub fn modal_dialog_present(node: &DomNode) -> bool {
  let mut stack: Vec<&DomNode> = vec![node];
  while let Some(node) = stack.pop() {
    if let Some((_, modal)) = dialog_state(node) {
      if modal {
        return true;
      }
    }

    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }

  false
}

fn set_attr(attrs: &mut Vec<(String, String)>, name: &str, value: &str) {
  if let Some((_, val)) = attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
    if val != value {
      val.clear();
      val.push_str(value);
    }
  } else {
    attrs.push((name.to_string(), value.to_string()));
  }
}

fn remove_attr(attrs: &mut Vec<(String, String)>, name: &str) {
  if let Some(idx) = attrs.iter().position(|(k, _)| k.eq_ignore_ascii_case(name)) {
    attrs.remove(idx);
  }
}

fn apply_top_layer_open_state_with_deadline(node: &mut DomNode) -> Result<bool> {
  let mut deadline_counter = 0usize;
  let mut modal_open = false;
  let mut stack = vec![node as *mut DomNode];

  while let Some(ptr) = stack.pop() {
    check_active_periodic(
      &mut deadline_counter,
      DOM_PARSE_NODE_DEADLINE_STRIDE,
      RenderStage::DomParse,
    )?;

    // Safety: `node` is mutably borrowed for the duration of this traversal, and we never mutate
    // the `children` vectors (only element attributes), so raw pointers remain stable.
    let current = unsafe { &mut *ptr };

    let dialog_info = dialog_state(current);
    if let Some((_, modal)) = dialog_info {
      modal_open |= modal;
    }

    let has_popover = current.get_attribute_ref("popover").is_some();
    let popover_is_open = has_popover && popover_open_assuming_popover(current);

    if let DomNodeType::Element {
      tag_name,
      attributes,
      ..
    } = &mut current.node_type
    {
      let is_dialog = tag_name.eq_ignore_ascii_case("dialog");
      let should_open = if is_dialog {
        dialog_info.is_some()
      } else if has_popover {
        popover_is_open
      } else {
        false
      };

      if is_dialog || has_popover {
        if should_open {
          set_attr(attributes, "open", "");
        } else {
          remove_attr(attributes, "open");
        }
      }
    }

    if current.is_template_element() {
      continue;
    }
    for child in current.children.iter_mut().rev() {
      stack.push(child as *mut DomNode);
    }
  }

  Ok(modal_open)
}

fn apply_top_layer_open_state(node: &mut DomNode) -> bool {
  let mut modal_open = false;
  let mut stack = vec![node as *mut DomNode];

  while let Some(ptr) = stack.pop() {
    // Safety: `node` is mutably borrowed for the duration of this traversal, and we never mutate
    // the `children` vectors (only element attributes), so raw pointers remain stable.
    let current = unsafe { &mut *ptr };

    let dialog_info = dialog_state(current);
    if let Some((_, modal)) = dialog_info {
      modal_open |= modal;
    }

    let has_popover = current.get_attribute_ref("popover").is_some();
    let popover_is_open = has_popover && popover_open_assuming_popover(current);

    if let DomNodeType::Element {
      tag_name,
      attributes,
      ..
    } = &mut current.node_type
    {
      let is_dialog = tag_name.eq_ignore_ascii_case("dialog");
      let should_open = if is_dialog {
        dialog_info.is_some()
      } else if has_popover {
        popover_is_open
      } else {
        false
      };

      if is_dialog || has_popover {
        if should_open {
          set_attr(attributes, "open", "");
        } else {
          remove_attr(attributes, "open");
        }
      }
    }

    if current.is_template_element() {
      continue;
    }
    for child in current.children.iter_mut().rev() {
      stack.push(child as *mut DomNode);
    }
  }

  modal_open
}

/// Bench helper: apply dialog/popover open state and `data-fastr-inert` propagation.
#[doc(hidden)]
pub fn apply_top_layer_state(node: &mut DomNode, modal_open: bool) {
  let _ = apply_top_layer_open_state(node);
  if modal_open {
    apply_top_layer_inert_state(node);
  }
}

fn open_modal_dialog_after_open_state(node: &DomNode) -> bool {
  let is_dialog = node
    .tag_name()
    .map(|t| t.eq_ignore_ascii_case("dialog"))
    .unwrap_or(false);
  if !is_dialog {
    return false;
  }
  if node.get_attribute_ref("open").is_none() {
    return false;
  }
  node
    .get_attribute_ref("data-fastr-modal")
    .map(boolish)
    .unwrap_or(false)
    || node
      .get_attribute_ref("data-fastr-open")
      .map(|v| v.eq_ignore_ascii_case("modal"))
      .unwrap_or(false)
}

fn apply_top_layer_inert_state_with_deadline(node: &mut DomNode) -> Result<()> {
  struct Frame {
    node: *mut DomNode,
    inside_modal: bool,
    entered: bool,
    next_child: usize,
    within_modal: bool,
    subtree_has_modal: bool,
  }

  let mut deadline_counter = 0usize;
  let mut stack = vec![Frame {
    node: node as *mut _,
    inside_modal: false,
    entered: false,
    next_child: 0,
    within_modal: false,
    subtree_has_modal: false,
  }];

  while let Some(mut frame) = stack.pop() {
    // Safety: `node` is mutably borrowed for the duration of the traversal, and we never mutate the
    // `children` vectors, so raw pointers remain stable for this post-order walk.
    let current = unsafe { &mut *frame.node };

    if !frame.entered {
      check_active_periodic(
        &mut deadline_counter,
        DOM_PARSE_NODE_DEADLINE_STRIDE,
        RenderStage::DomParse,
      )?;

      frame.entered = true;
      frame.within_modal = frame.inside_modal;
      frame.subtree_has_modal = frame.within_modal;

      if !frame.within_modal {
        if open_modal_dialog_after_open_state(current) {
          frame.within_modal = true;
          frame.subtree_has_modal = true;
        }
      }
    }

    let child_len = if current.is_template_element() {
      0
    } else {
      current.children.len()
    };
    if frame.next_child < child_len {
      let child_ptr = &mut current.children[frame.next_child] as *mut DomNode;
      frame.next_child += 1;
      let child_inside_modal = frame.within_modal;

      stack.push(frame);
      stack.push(Frame {
        node: child_ptr,
        inside_modal: child_inside_modal,
        entered: false,
        next_child: 0,
        within_modal: false,
        subtree_has_modal: false,
      });
      continue;
    }

    if let DomNodeType::Element { attributes, .. } = &mut current.node_type {
      if !frame.subtree_has_modal {
        set_attr(attributes, "data-fastr-inert", "true");
      }
    }

    let subtree_has_modal = frame.subtree_has_modal;
    if let Some(parent) = stack.last_mut() {
      parent.subtree_has_modal |= subtree_has_modal;
    }
  }

  Ok(())
}

fn apply_top_layer_inert_state(node: &mut DomNode) {
  struct Frame {
    node: *mut DomNode,
    inside_modal: bool,
    entered: bool,
    next_child: usize,
    within_modal: bool,
    subtree_has_modal: bool,
  }

  let mut stack = vec![Frame {
    node: node as *mut _,
    inside_modal: false,
    entered: false,
    next_child: 0,
    within_modal: false,
    subtree_has_modal: false,
  }];

  while let Some(mut frame) = stack.pop() {
    // Safety: `node` is mutably borrowed for the duration of the traversal, and we never mutate the
    // `children` vectors, so raw pointers remain stable for this post-order walk.
    let current = unsafe { &mut *frame.node };

    if !frame.entered {
      frame.entered = true;
      frame.within_modal = frame.inside_modal;
      frame.subtree_has_modal = frame.within_modal;

      if !frame.within_modal {
        if open_modal_dialog_after_open_state(current) {
          frame.within_modal = true;
          frame.subtree_has_modal = true;
        }
      }
    }

    let child_len = if current.is_template_element() {
      0
    } else {
      current.children.len()
    };
    if frame.next_child < child_len {
      let child_ptr = &mut current.children[frame.next_child] as *mut DomNode;
      frame.next_child += 1;
      let child_inside_modal = frame.within_modal;

      stack.push(frame);
      stack.push(Frame {
        node: child_ptr,
        inside_modal: child_inside_modal,
        entered: false,
        next_child: 0,
        within_modal: false,
        subtree_has_modal: false,
      });
      continue;
    }

    if let DomNodeType::Element { attributes, .. } = &mut current.node_type {
      if !frame.subtree_has_modal {
        set_attr(attributes, "data-fastr-inert", "true");
      }
    }

    let subtree_has_modal = frame.subtree_has_modal;
    if let Some(parent) = stack.last_mut() {
      parent.subtree_has_modal |= subtree_has_modal;
    }
  }
}

/// Applies dialog/popover open state, then propagates `data-fastr-inert` if a modal dialog is open.
///
/// This is a benchmarking helper mirroring the render pipeline's top-layer preprocessing
/// without deadline checks.
#[doc(hidden)]
pub fn apply_top_layer_state_auto(node: &mut DomNode) -> bool {
  let modal_open = apply_top_layer_open_state(node);
  if modal_open {
    apply_top_layer_inert_state(node);
  }
  modal_open
}

pub(crate) fn apply_top_layer_state_with_deadline(node: &mut DomNode) -> Result<()> {
  // Apply `<dialog>`/`[popover]` open state in a single pre-order walk while detecting whether the
  // document contains an open modal dialog. Inert propagation depends on knowing that global modal
  // state, so we only run the heavier post-order inert pass when needed.
  let modal_open = apply_top_layer_open_state_with_deadline(node)?;
  if modal_open {
    apply_top_layer_inert_state_with_deadline(node)?;
  }
  Ok(())
}

pub fn parse_html(html: &str) -> Result<DomNode> {
  parse_html_with_options(html, DomParseOptions::default())
}

fn map_quirks_mode(mode: HtmlQuirksMode) -> QuirksMode {
  match mode {
    HtmlQuirksMode::Quirks => QuirksMode::Quirks,
    HtmlQuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
    HtmlQuirksMode::NoQuirks => QuirksMode::NoQuirks,
  }
}

struct DeadlineCheckedRead<R> {
  inner: R,
  deadline_counter: usize,
}

impl<R> DeadlineCheckedRead<R> {
  fn new(inner: R) -> Self {
    Self {
      inner,
      deadline_counter: 0,
    }
  }
}

impl<R: io::Read> io::Read for DeadlineCheckedRead<R> {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    if let Err(err) = check_active_periodic(
      &mut self.deadline_counter,
      DOM_PARSE_READ_DEADLINE_STRIDE,
      RenderStage::DomParse,
    ) {
      return Err(io::Error::new(io::ErrorKind::TimedOut, err));
    }

    let len = buf.len().min(DOM_PARSE_READ_MAX_CHUNK_BYTES);
    self.inner.read(&mut buf[..len])
  }
}

/// Parse HTML with explicit parsing options (e.g., DOM compatibility mode).
pub fn parse_html_with_options(html: &str, options: DomParseOptions) -> Result<DomNode> {
  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: options.scripting_enabled,
      ..Default::default()
    },
    ..Default::default()
  };

  let html5ever_timer = dom_parse_diagnostics_timer();
  let reader = io::Cursor::new(html.as_bytes());
  let mut reader = DeadlineCheckedRead::new(reader);
  let dom = parse_document(RcDom::default(), opts)
    .from_utf8()
    .read_from(&mut reader)
    .map_err(|e| {
      if e.kind() == io::ErrorKind::TimedOut {
        if let Some(timeout) = e
          .get_ref()
          .and_then(|inner| inner.downcast_ref::<crate::error::RenderError>())
        {
          return Error::Render(timeout.clone());
        }
        return Error::Render(crate::error::RenderError::Timeout {
          stage: RenderStage::DomParse,
          elapsed: crate::render_control::active_deadline()
            .as_ref()
            .map(|deadline| deadline.elapsed())
            .unwrap_or_default(),
        });
      }

      Error::Parse(ParseError::InvalidHtml {
        message: format!("Failed to parse HTML: {}", e),
        line: 0,
      })
    })?;
  if let Some(start) = html5ever_timer {
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    with_dom_parse_diagnostics(|diag| {
      diag.html5ever_ms += elapsed_ms;
    });
  }

  let quirks_mode = map_quirks_mode(dom.quirks_mode.get());

  fn convert_document_handle_to_root(
    handle: &Handle,
    quirks_mode: QuirksMode,
    deadline_counter: &mut usize,
  ) -> Result<DomNode> {
    convert_handle_to_node(handle, quirks_mode, deadline_counter)?.ok_or_else(|| {
      Error::Parse(ParseError::InvalidHtml {
        message: "DOM conversion produced no document root node".to_string(),
        line: 0,
      })
    })
  }

  let convert_timer = dom_parse_diagnostics_timer();
  let mut deadline_counter = 0usize;
  let mut root = convert_document_handle_to_root(&dom.document, quirks_mode, &mut deadline_counter)?;
  if let Some(start) = convert_timer {
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    with_dom_parse_diagnostics(|diag| {
      diag.convert_ms += elapsed_ms;
    });
  }

  let shadow_attach_timer = dom_parse_diagnostics_timer();
  attach_shadow_roots(&mut root, &mut deadline_counter)?;
  if let Some(start) = shadow_attach_timer {
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    with_dom_parse_diagnostics(|diag| {
      diag.shadow_attach_ms += elapsed_ms;
    });
  }

  if matches!(
    options.compatibility_mode,
    DomCompatibilityMode::Compatibility
  ) {
    let compat_timer = dom_parse_diagnostics_timer();
    apply_dom_compatibility_mutations(&mut root, &mut deadline_counter)?;
    if let Some(start) = compat_timer {
      let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
      with_dom_parse_diagnostics(|diag| {
        diag.compat_ms += elapsed_ms;
      });
    }
  }

  Ok(root)
}

/// Clone a DOM tree while periodically checking any active render deadline.
///
/// Unlike `DomNode::clone`, this uses an explicit stack so deeply nested documents don't risk a
/// stack overflow and so `RenderOptions::timeout` can abort pathological `dom_parse` workloads
/// cooperatively.
pub(crate) fn clone_dom_with_deadline(node: &DomNode, stage: RenderStage) -> Result<DomNode> {
  struct Frame {
    src: *const DomNode,
    dst: *mut DomNode,
    next_child: usize,
  }

  let mut deadline_counter = 0usize;
  check_active_periodic(&mut deadline_counter, DOM_PARSE_NODE_DEADLINE_STRIDE, stage)?;

  let mut root = DomNode {
    node_type: node.node_type.clone(),
    children: Vec::with_capacity(node.children.len()),
  };

  let mut stack = vec![Frame {
    src: node as *const _,
    dst: &mut root as *mut _,
    next_child: 0,
  }];

  while let Some(mut frame) = stack.pop() {
    let src = unsafe { &*frame.src };
    // Safety: destination nodes are owned by `root` and its descendants, and we never mutate a
    // node's children while a frame borrowing that node is active. This keeps raw pointers stable
    // for the duration of the DFS clone.
    let dst = unsafe { &mut *frame.dst };

    if frame.next_child < src.children.len() {
      let child_src = &src.children[frame.next_child];
      frame.next_child += 1;

      check_active_periodic(&mut deadline_counter, DOM_PARSE_NODE_DEADLINE_STRIDE, stage)?;

      dst.children.push(DomNode {
        node_type: child_src.node_type.clone(),
        children: Vec::with_capacity(child_src.children.len()),
      });
      let child_dst = dst
        .children
        .last_mut()
        .map(|node| node as *mut DomNode)
        .ok_or_else(|| Error::Other("clone_dom_with_deadline: child node missing after push".into()))?;

      stack.push(frame);
      stack.push(Frame {
        src: child_src as *const _,
        dst: child_dst,
        next_child: 0,
      });
    }
  }

  Ok(root)
}

/// Clone a DOM tree and also report whether top-layer state (dialog/popover open state + inert
/// propagation) is needed.
///
/// This is used to avoid an additional full-tree `needs_top_layer_state` scan in call sites that
/// already pay the cost of cloning the DOM.
pub(crate) fn clone_dom_with_deadline_and_top_layer_hint(
  node: &DomNode,
  stage: RenderStage,
) -> Result<(DomNode, bool)> {
  struct Frame {
    src: *const DomNode,
    dst: *mut DomNode,
    next_child: usize,
    scan_children_suppressed: bool,
  }

  fn scan_top_layer_hint(node: &DomNode, hint: &mut bool, suppressed: bool) -> bool {
    if suppressed {
      return true;
    }
    if *hint {
      return false;
    }

    match &node.node_type {
      DomNodeType::ShadowRoot { .. } => {
        // `needs_top_layer_state` treats shadow DOM presence as a reason to run the full traversal
        // because dialogs/popovers may appear inside attached shadow roots.
        *hint = true;
        false
      }
      DomNodeType::Element {
        tag_name,
        namespace,
        attributes,
      } => {
        if (namespace.is_empty() || namespace == HTML_NAMESPACE)
          && tag_name.eq_ignore_ascii_case("dialog")
        {
          *hint = true;
        }

        // Template contents are inert; avoid descending into them to match `needs_top_layer_state`.
        if tag_name.eq_ignore_ascii_case("template") {
          return true;
        }

        for (name, _) in attributes {
          if name.eq_ignore_ascii_case("popover")
            || name.eq_ignore_ascii_case("data-fastr-open")
            || name.eq_ignore_ascii_case("data-fastr-modal")
          {
            *hint = true;
            break;
          }
        }
        false
      }
      DomNodeType::Slot { attributes, .. } => {
        for (name, _) in attributes {
          if name.eq_ignore_ascii_case("popover")
            || name.eq_ignore_ascii_case("data-fastr-open")
            || name.eq_ignore_ascii_case("data-fastr-modal")
          {
            *hint = true;
            break;
          }
        }
        false
      }
      DomNodeType::Document { .. } | DomNodeType::Text { .. } => false,
    }
  }

  let mut deadline_counter = 0usize;
  check_active_periodic(&mut deadline_counter, DOM_PARSE_NODE_DEADLINE_STRIDE, stage)?;

  let mut top_layer_hint = false;
  let root_children_suppressed = scan_top_layer_hint(node, &mut top_layer_hint, false);

  let mut root = DomNode {
    node_type: node.node_type.clone(),
    children: Vec::with_capacity(node.children.len()),
  };

  let mut stack = vec![Frame {
    src: node as *const _,
    dst: &mut root as *mut _,
    next_child: 0,
    scan_children_suppressed: root_children_suppressed,
  }];

  while let Some(mut frame) = stack.pop() {
    let src = unsafe { &*frame.src };
    // Safety: destination nodes are owned by `root` and its descendants, and we never mutate a
    // node's children while a frame borrowing that node is active. This keeps raw pointers stable
    // for the duration of the DFS clone.
    let dst = unsafe { &mut *frame.dst };

    if frame.next_child < src.children.len() {
      let child_src = &src.children[frame.next_child];
      frame.next_child += 1;

      check_active_periodic(&mut deadline_counter, DOM_PARSE_NODE_DEADLINE_STRIDE, stage)?;

      dst.children.push(DomNode {
        node_type: child_src.node_type.clone(),
        children: Vec::with_capacity(child_src.children.len()),
      });
      let child_dst = dst
        .children
        .last_mut()
        .map(|node| node as *mut DomNode)
        .ok_or_else(|| {
          Error::Other("clone_dom_with_deadline_and_top_layer_hint: child node missing after push".into())
        })?;

      let child_scan_suppressed = frame.scan_children_suppressed;
      let child_children_suppressed = if top_layer_hint {
        false
      } else {
        scan_top_layer_hint(child_src, &mut top_layer_hint, child_scan_suppressed)
      };

      stack.push(frame);
      stack.push(Frame {
        src: child_src as *const _,
        dst: child_dst,
        next_child: 0,
        scan_children_suppressed: child_children_suppressed,
      });
    }
  }

  Ok((root, top_layer_hint))
}

fn parse_shadow_root_definition(template: &DomNode) -> Option<(ShadowRootMode, bool)> {
  // Declarative shadow DOM only applies to HTML templates, not e.g. SVG <template>.
  if !template.is_html_template_element() {
    return None;
  }

  let mode_attr = template
    .get_attribute_ref("shadowroot")
    .or_else(|| template.get_attribute_ref("shadowrootmode"))?;
  let mode = if mode_attr.eq_ignore_ascii_case("open") {
    ShadowRootMode::Open
  } else if mode_attr.eq_ignore_ascii_case("closed") {
    ShadowRootMode::Closed
  } else {
    return None;
  };

  let delegates_focus = template
    .get_attribute_ref("shadowrootdelegatesfocus")
    .is_some();

  Some((mode, delegates_focus))
}

fn attach_shadow_roots(node: &mut DomNode, deadline_counter: &mut usize) -> Result<()> {
  // `attach_shadow_roots` needs to run in post-order so shadow root templates are promoted after
  // their template contents have been scanned (allowing nested declarative shadow roots inside the
  // template).
  let mut stack: Vec<(*mut DomNode, bool)> = Vec::new();
  stack.push((node as *mut DomNode, false));

  while let Some((ptr, after_children)) = stack.pop() {
    // Safety: all pointers are into the `node` (root) tree and we only mutate a node's `children`
    // vec in the `after_children` phase, after all child frames have been processed.
    let node = unsafe { &mut *ptr };

    if !after_children {
      check_active_periodic(
        deadline_counter,
        DOM_PARSE_NODE_DEADLINE_STRIDE,
        RenderStage::DomParse,
      )?;

      stack.push((ptr, true));

      // Declarative shadow DOM only promotes the first shadow root template child of a shadow host
      // element. Additional `<template shadowroot=...>` siblings must remain inert, so we skip
      // traversing into them here.
      let first_declarative_shadow_template = if node.is_element() && !node.is_template_element() {
        node
          .children
          .iter()
          .position(|child| parse_shadow_root_definition(child).is_some())
      } else {
        None
      };
      let len = node.children.len();
      let children_ptr = node.children.as_mut_ptr();
      for idx in (0..len).rev() {
        let child_ptr = unsafe { children_ptr.add(idx) };
        let child = unsafe { &*child_ptr };
        // Template contents are inert; only the first declarative shadow DOM template is walked so
        // nested declarative shadow roots inside it can be promoted.
        if child.is_template_element() && first_declarative_shadow_template != Some(idx) {
          continue;
        }

        stack.push((child_ptr, false));
      }

      continue;
    }

    if !node.is_element() || node.is_template_element() {
      continue;
    }

    let mut shadow_template = None;
    for (idx, child) in node.children.iter().enumerate() {
      if let Some((mode, delegates_focus)) = parse_shadow_root_definition(child) {
        shadow_template = Some((idx, mode, delegates_focus));
        break;
      }
    }

    let Some((template_idx, mode, delegates_focus)) = shadow_template else {
      continue;
    };

    // Only the first declarative shadow template is promoted to a shadow root, matching browsers.
    // Subsequent templates remain as inert light DOM children.
    let mut template = node.children.remove(template_idx);
    let template_children = std::mem::take(&mut template.children);
    let shadow_root = DomNode {
      node_type: DomNodeType::ShadowRoot {
        mode,
        delegates_focus,
      },
      children: template_children,
    };
    let light_children = std::mem::take(&mut node.children);
    node.children = {
      let mut combined = Vec::with_capacity(light_children.len() + 1);
      combined.push(shadow_root);
      combined.extend(light_children);
      combined
    };
  }

  Ok(())
}

fn collect_slot_names<'a>(
  node: &'a DomNode,
  out: &mut HashSet<&'a str>,
  deadline_counter: &mut usize,
) -> Result<()> {
  let root_ptr = node as *const DomNode;
  let mut stack: Vec<&'a DomNode> = Vec::new();
  stack.push(node);

  while let Some(current) = stack.pop() {
    check_active_periodic(
      deadline_counter,
      SHADOW_MAP_DEADLINE_STRIDE,
      RenderStage::Cascade,
    )
    .map_err(Error::Render)?;
    if matches!(current.node_type, DomNodeType::ShadowRoot { .. }) && !ptr::eq(current, root_ptr) {
      // Slot assignment is scoped to a single shadow root; do not treat slots inside nested shadow
      // roots as "available" when assigning this host's light DOM children.
      continue;
    }

    if matches!(current.node_type, DomNodeType::Slot { .. }) {
      out.insert(current.get_attribute_ref("name").unwrap_or(""));
    }

    for child in current.traversal_children().iter().rev() {
      if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      stack.push(child);
    }
  }
  Ok(())
}

fn take_assignments_for_slot_ptr(
  assignments: &mut Vec<(Option<&str>, *const DomNode)>,
  slot_name: &str,
  available_slots: &HashSet<&str>,
  deadline_counter: &mut usize,
) -> Result<Vec<*const DomNode>> {
  let mut taken = Vec::new();
  let mut write = 0usize;
  for read in 0..assignments.len() {
    check_active_periodic(
      deadline_counter,
      SHADOW_MAP_DEADLINE_STRIDE,
      RenderStage::Cascade,
    )
    .map_err(Error::Render)?;
    let (name, node) = assignments[read];
    let target = name.unwrap_or("");
    let matches = if slot_name.is_empty() {
      name.is_none() || !available_slots.contains(target)
    } else {
      target == slot_name
    };

    if matches {
      taken.push(node);
    } else {
      assignments[write] = (name, node);
      write += 1;
    }
  }
  assignments.truncate(write);
  Ok(taken)
}

fn fill_slot_assignments(
  node: &DomNode,
  shadow_root_id: usize,
  assignments: &mut Vec<(Option<&str>, *const DomNode)>,
  available_slots: &HashSet<&str>,
  ids: &HashMap<*const DomNode, usize>,
  out: &mut SlotAssignment,
  deadline_counter: &mut usize,
) -> Result<()> {
  let root_ptr = node as *const DomNode;
  let mut stack: Vec<&DomNode> = Vec::new();
  stack.push(node);

  while let Some(current) = stack.pop() {
    check_active_periodic(
      deadline_counter,
      SHADOW_MAP_DEADLINE_STRIDE,
      RenderStage::Cascade,
    )
    .map_err(Error::Render)?;
    let mut traverse_children = true;

    if matches!(current.node_type, DomNodeType::ShadowRoot { .. }) && !ptr::eq(current, root_ptr) {
      // Shadow tree boundaries block assignment of this host's light DOM into nested shadow roots.
      traverse_children = false;
    }

    if matches!(current.node_type, DomNodeType::Slot { .. }) {
      let slot_name = current.get_attribute_ref("name").unwrap_or("");
      let assigned =
        take_assignments_for_slot_ptr(assignments, slot_name, available_slots, deadline_counter)?;
      if !assigned.is_empty() {
        let slot_id = ids.get(&(current as *const DomNode)).copied().unwrap_or(0);
        let mut assigned_ids: Vec<usize> = Vec::with_capacity(assigned.len());
        for ptr in assigned.iter() {
          check_active_periodic(
            deadline_counter,
            SHADOW_MAP_DEADLINE_STRIDE,
            RenderStage::Cascade,
          )
          .map_err(Error::Render)?;
          if let Some(id) = ids.get(ptr).copied() {
            assigned_ids.push(id);
          }
        }
        out
          .shadow_to_slots
          .entry(shadow_root_id)
          .or_default()
          .entry(slot_name.to_string())
          .or_default()
          .extend(assigned_ids.iter().copied());
        for &node_id in &assigned_ids {
          check_active_periodic(
            deadline_counter,
            SHADOW_MAP_DEADLINE_STRIDE,
            RenderStage::Cascade,
          )
          .map_err(Error::Render)?;
          out.node_to_slot.insert(
            node_id,
            AssignedSlot {
              slot_name: slot_name.to_string(),
              slot_node_id: slot_id,
              shadow_root_id,
            },
          );
        }
        out.slot_to_nodes.insert(slot_id, assigned_ids);
        // Once a slot is assigned, its fallback subtree is not rendered; do not traverse children.
        traverse_children = false;
      }
    }

    if traverse_children {
      for child in current.traversal_children().iter().rev() {
        if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
          continue;
        }
        stack.push(child);
      }
    }
  }
  Ok(())
}

fn enumerate_node_ids(node: &DomNode, next: &mut usize, map: &mut HashMap<*const DomNode, usize>) {
  let mut stack: Vec<&DomNode> = Vec::new();
  stack.push(node);

  while let Some(current) = stack.pop() {
    map.insert(current as *const DomNode, *next);
    *next += 1;
    for child in current.children.iter().rev() {
      stack.push(child);
    }
  }
}

/// Assign stable pre-order traversal ids to each node in the DOM tree.
pub fn enumerate_dom_ids(root: &DomNode) -> HashMap<*const DomNode, usize> {
  let mut ids: HashMap<*const DomNode, usize> = HashMap::new();
  let mut next_id = 1usize;
  enumerate_node_ids(root, &mut next_id, &mut ids);
  ids
}

/// Find a mutable reference to the node with the given stable pre-order id.
///
/// The id scheme matches [`enumerate_dom_ids`]: ids are 1-based and assigned by a depth-first
/// pre-order traversal.
///
/// This helper uses an explicit stack to avoid stack overflows on extremely deep/degenerate trees.
pub fn find_node_mut_by_preorder_id(root: &mut DomNode, id: usize) -> Option<&mut DomNode> {
  if id == 0 {
    return None;
  }

  let mut next_id = 1usize;
  let mut stack: Vec<*mut DomNode> = Vec::new();
  stack.push(root as *mut DomNode);

  while let Some(ptr) = stack.pop() {
    // Safety: `root` is mutably borrowed for the duration of this search, and we do not mutate any
    // `children` vectors while raw pointers are stored in `stack`, so pointers remain valid.
    let current = unsafe { &mut *ptr };
    if next_id == id {
      return Some(current);
    }
    next_id += 1;

    for child in current.children.iter_mut().rev() {
      stack.push(child as *mut DomNode);
    }
  }

  None
}

/// Compute the slot assignment map for all shadow roots in the DOM.
pub fn compute_slot_assignment(root: &DomNode) -> Result<SlotAssignment> {
  let ids = enumerate_dom_ids(root);
  compute_slot_assignment_with_ids(root, &ids)
}

/// Compute the slot assignment map for all shadow roots in the DOM using a precomputed id map.
pub fn compute_slot_assignment_with_ids(
  root: &DomNode,
  ids: &HashMap<*const DomNode, usize>,
) -> Result<SlotAssignment> {
  let mut assignment = SlotAssignment::default();

  let mut stack: Vec<(&DomNode, Option<&DomNode>)> = Vec::with_capacity(ids.len().min(1024));
  stack.push((root, None));
  let mut deadline_counter = 0usize;

  while let Some((node, parent)) = stack.pop() {
    check_active_periodic(
      &mut deadline_counter,
      SHADOW_MAP_DEADLINE_STRIDE,
      RenderStage::Cascade,
    )
    .map_err(Error::Render)?;
    if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
      if let Some(host) = parent {
        let mut available_slots: HashSet<&str> = HashSet::new();
        collect_slot_names(node, &mut available_slots, &mut deadline_counter)?;
        let mut light_children: Vec<(Option<&str>, *const DomNode)> =
          Vec::with_capacity(host.children.len());
        for child in host
          .children
          .iter()
          .filter(|c| !matches!(c.node_type, DomNodeType::ShadowRoot { .. }))
        {
          check_active_periodic(
            &mut deadline_counter,
            SHADOW_MAP_DEADLINE_STRIDE,
            RenderStage::Cascade,
          )
          .map_err(Error::Render)?;
          let slot_name = child
            .get_attribute_ref("slot")
            .map(trim_ascii_whitespace_html)
            .filter(|v| !v.is_empty());
          light_children.push((slot_name, child as *const DomNode));
        }

        let shadow_root_id = ids.get(&(node as *const DomNode)).copied().unwrap_or(0);
        fill_slot_assignments(
          node,
          shadow_root_id,
          &mut light_children,
          &available_slots,
          ids,
          &mut assignment,
          &mut deadline_counter,
        )?;
      }
    }

    for child in node.traversal_children().iter().rev() {
      stack.push((child, Some(node)));
    }
  }
  Ok(assignment)
}

/// Create a composed-tree view of the DOM using precomputed node IDs and slot assignment.
///
/// This is a snapshot-only transformation; it does not mutate the input DOM and does not require
/// style/layout.
#[doc(hidden)]
pub fn composed_dom_snapshot_with_ids_and_assignment(
  root: &DomNode,
  ids: &HashMap<*const DomNode, usize>,
  assignment: &SlotAssignment,
) -> Result<DomNode> {
  const COMPOSED_SNAPSHOT_DEADLINE_STRIDE: usize = 1024;

  let mut id_to_node: Vec<*const DomNode> = vec![ptr::null(); ids.len() + 1];
  for (ptr, id) in ids.iter() {
    if *id < id_to_node.len() {
      id_to_node[*id] = *ptr;
    }
  }

  struct Frame {
    out: DomNode,
    children: Vec<*const DomNode>,
    next_child: usize,
  }

  fn composed_children_for(
    src: &DomNode,
    src_ptr: *const DomNode,
    ids: &HashMap<*const DomNode, usize>,
    id_to_node: &[*const DomNode],
    assignment: &SlotAssignment,
    out: &mut DomNode,
    deadline_counter: &mut usize,
  ) -> Result<Vec<*const DomNode>> {
    if src.is_shadow_host() {
      let mut shadow_root: Option<&DomNode> = None;
      for child in src.children.iter() {
        check_active_periodic(
          deadline_counter,
          COMPOSED_SNAPSHOT_DEADLINE_STRIDE,
          RenderStage::DomParse,
        )?;
        if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
          shadow_root = Some(child);
          break;
        }
      }
      if let Some(shadow_root) = shadow_root {
        // In the composed tree, the shadow root's children replace the host's light DOM children.
        // The `ShadowRoot` node itself is an internal representation detail of the parsed DOM and
        // is not exposed as part of the composed snapshot.
        let children = shadow_root.traversal_children();
        let mut out_children = Vec::with_capacity(children.len());
        for child in children {
          check_active_periodic(
            deadline_counter,
            COMPOSED_SNAPSHOT_DEADLINE_STRIDE,
            RenderStage::DomParse,
          )?;
          out_children.push(child as *const DomNode);
        }
        return Ok(out_children);
      }
    }

    if matches!(src.node_type, DomNodeType::Slot { .. }) {
      let slot_id = ids.get(&src_ptr).copied().unwrap_or(0);
      if let Some(assigned_ids) = assignment.slot_to_nodes.get(&slot_id) {
        if !assigned_ids.is_empty() {
          if let DomNodeType::Slot { assigned, .. } = &mut out.node_type {
            *assigned = true;
          }
          let mut children = Vec::with_capacity(assigned_ids.len());
          for node_id in assigned_ids.iter() {
            check_active_periodic(
              deadline_counter,
              COMPOSED_SNAPSHOT_DEADLINE_STRIDE,
              RenderStage::DomParse,
            )?;
            let Some(ptr) = id_to_node.get(*node_id).copied() else {
              continue;
            };
            if ptr.is_null() {
              continue;
            }
            children.push(ptr);
          }
          return Ok(children);
        }
      }

      if let DomNodeType::Slot { assigned, .. } = &mut out.node_type {
        *assigned = false;
      }
      let children = src.traversal_children();
      let mut out_children = Vec::with_capacity(children.len());
      for child in children {
        check_active_periodic(
          deadline_counter,
          COMPOSED_SNAPSHOT_DEADLINE_STRIDE,
          RenderStage::DomParse,
        )?;
        out_children.push(child as *const DomNode);
      }
      return Ok(out_children);
    }

    let children = src.traversal_children();
    let mut out_children = Vec::with_capacity(children.len());
    for child in children {
      check_active_periodic(
        deadline_counter,
        COMPOSED_SNAPSHOT_DEADLINE_STRIDE,
        RenderStage::DomParse,
      )?;
      out_children.push(child as *const DomNode);
    }
    Ok(out_children)
  }

  let mut deadline_counter = 0usize;

  let root_ptr = root as *const DomNode;
  let mut out_root = root.clone_without_children();
  let root_children = composed_children_for(
    root,
    root_ptr,
    ids,
    &id_to_node,
    assignment,
    &mut out_root,
    &mut deadline_counter,
  )?;
  let mut stack = vec![Frame {
    out: out_root,
    children: root_children,
    next_child: 0,
  }];

  while let Some(frame) = stack.last_mut() {
    check_active_periodic(
      &mut deadline_counter,
      COMPOSED_SNAPSHOT_DEADLINE_STRIDE,
      RenderStage::DomParse,
    )?;

    if frame.next_child < frame.children.len() {
      let child_ptr = frame.children[frame.next_child];
      frame.next_child += 1;
      // Safety: pointers are to nodes owned by `root`, which is immutable for the duration of this
      // traversal.
      let child = unsafe { &*child_ptr };
      let mut out_child = child.clone_without_children();
      let child_children = composed_children_for(
        child,
        child_ptr,
        ids,
        &id_to_node,
        assignment,
        &mut out_child,
        &mut deadline_counter,
      )?;
      stack.push(Frame {
        out: out_child,
        children: child_children,
        next_child: 0,
      });
      continue;
    }

    let finished = match stack.pop() {
      Some(frame) => frame.out,
      None => {
        return Err(Error::Other(
          "composed_dom_snapshot: traversal stack unexpectedly empty".to_string(),
        ))
      }
    };
    if let Some(parent) = stack.last_mut() {
      parent.out.children.push(finished);
    } else {
      return Ok(finished);
    }
  }

  Err(Error::Other(
    "composed_dom_snapshot: traversal stack unexpectedly empty".to_string(),
  ))
}

/// Create a composed-tree view of the DOM for debugging and tooling.
///
/// The returned tree represents the DOM as it would appear after:
/// - Shadow DOM: a shadow host's children are replaced by its shadow root's children.
/// - Slotting: `<slot>` elements expand to their assigned nodes (or fallback children when empty).
///
/// This is a snapshot-only transformation; it does not mutate the input DOM and does not require
/// style/layout.
pub fn composed_dom_snapshot(root: &DomNode) -> Result<DomNode> {
  let ids = enumerate_dom_ids(root);
  let assignment = compute_slot_assignment_with_ids(root, &ids)?;
  composed_dom_snapshot_with_ids_and_assignment(root, &ids, &assignment)
}

fn push_part_export(
  exports: &mut HashMap<String, Vec<ExportedPartTarget>>,
  name: &str,
  target: ExportedPartTarget,
) {
  let entry = exports.entry(name.to_string()).or_default();
  if !entry.contains(&target) {
    entry.push(target);
  }
}

/// Compute the mapping of exported parts for each shadow host in the DOM.
///
/// Closed shadow roots are still traversed here because declarative snapshots include their
/// contents, and `exportparts` is an explicit opt-in for styling from the outside.
pub fn compute_part_export_map(root: &DomNode) -> Result<PartExportMap> {
  let ids = enumerate_dom_ids(root);
  compute_part_export_map_with_ids(root, &ids)
}

/// Compute the mapping of exported parts for each shadow host using a precomputed id map.
pub fn compute_part_export_map_with_ids(
  root: &DomNode,
  ids: &HashMap<*const DomNode, usize>,
) -> Result<PartExportMap> {
  // Collect shadow hosts in pre-order, then compute exports in reverse (leaf-to-root) order so
  // nested shadow host exports are available when processing their ancestors.
  let mut hosts: Vec<&DomNode> = Vec::new();
  let mut traversal_stack: Vec<&DomNode> = Vec::with_capacity(ids.len().min(1024));
  traversal_stack.push(root);
  let mut deadline_counter = 0usize;

  while let Some(node) = traversal_stack.pop() {
    check_active_periodic(
      &mut deadline_counter,
      SHADOW_MAP_DEADLINE_STRIDE,
      RenderStage::Cascade,
    )
    .map_err(Error::Render)?;
    if node.is_shadow_host() {
      hosts.push(node);
    }

    for child in node.traversal_children().iter().rev() {
      traversal_stack.push(child);
    }
  }

  let mut map = PartExportMap::default();

  for host in hosts.into_iter().rev() {
    check_active_periodic(
      &mut deadline_counter,
      SHADOW_MAP_DEADLINE_STRIDE,
      RenderStage::Cascade,
    )
    .map_err(Error::Render)?;
    let host_id = ids.get(&(host as *const DomNode)).copied().unwrap_or(0);
    if map.exports_for_host(host_id).is_some() {
      continue;
    }

    let shadow_root = host
      .children
      .iter()
      .find(|child| matches!(child.node_type, DomNodeType::ShadowRoot { .. }));
    let Some(shadow_root) = shadow_root else {
      continue;
    };

    let mut exports: HashMap<String, Vec<ExportedPartTarget>> = HashMap::new();

    let mut shadow_stack: Vec<&DomNode> = Vec::new();
    shadow_stack.push(shadow_root);

    while let Some(node) = shadow_stack.pop() {
      check_active_periodic(
        &mut deadline_counter,
        SHADOW_MAP_DEADLINE_STRIDE,
        RenderStage::Cascade,
      )
      .map_err(Error::Render)?;
      if let Some(parts) = node.get_attribute_ref("part") {
        let node_id = ids.get(&(node as *const DomNode)).copied().unwrap_or(0);
        for part in parts.split_ascii_whitespace() {
          check_active_periodic(
            &mut deadline_counter,
            SHADOW_MAP_DEADLINE_STRIDE,
            RenderStage::Cascade,
          )
          .map_err(Error::Render)?;
          push_part_export(&mut exports, part, ExportedPartTarget::Element(node_id));
        }
      }

      if let Some(mapping) = node.get_attribute_ref("exportparts") {
        let node_id = ids.get(&(node as *const DomNode)).copied().unwrap_or(0);
        for (internal, alias) in parse_exportparts(mapping) {
          check_active_periodic(
            &mut deadline_counter,
            SHADOW_MAP_DEADLINE_STRIDE,
            RenderStage::Cascade,
          )
          .map_err(Error::Render)?;
          let Some(pseudo) = exportparts_exportable_pseudo(&internal) else {
            continue;
          };
          // `exportparts` pseudo forwarding requires an explicit outer ident; ignore identity/invalid
          // mappings like `::before` or `::before:` which would surface as `alias == internal`.
          if alias.starts_with("::") {
            continue;
          }
          push_part_export(
            &mut exports,
            &alias,
            ExportedPartTarget::Pseudo { node_id, pseudo },
          );
        }
      }

      if node.is_shadow_host() {
        let nested_id = ids.get(&(node as *const DomNode)).copied().unwrap_or(0);
        // Nested shadow contents are only exposed when the nested host opts in via `exportparts`.
        if node.get_attribute_ref("exportparts").is_some() {
          if let Some(nested_exports) = map.exports_for_host(nested_id) {
            for (name, targets) in nested_exports.iter() {
              for target in targets {
                check_active_periodic(
                  &mut deadline_counter,
                  SHADOW_MAP_DEADLINE_STRIDE,
                  RenderStage::Cascade,
                )
                .map_err(Error::Render)?;
                push_part_export(&mut exports, name, target.clone());
              }
            }
          }
        }
      }

      for child in node.traversal_children().iter().rev() {
        if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
          // Nested shadow contents are only exposed via exportparts on the host.
          continue;
        }
        shadow_stack.push(child);
      }
    }

    if let Some(exportparts) = host.get_attribute_ref("exportparts") {
      // Per CSS Shadow Parts, `exportparts` acts as an allowlist: when present, only the mapped
      // part names are exported across this shadow boundary.
      //
      // Apply the mapping against the pre-mapping `exports` table so mappings cannot chain within a
      // single attribute value (e.g. `a:b, b:c` must not export `a` as `c` when there is no `b`).
      let mut mapped: HashMap<String, Vec<ExportedPartTarget>> = HashMap::new();
      for (internal, alias) in parse_exportparts(exportparts) {
        check_active_periodic(
          &mut deadline_counter,
          SHADOW_MAP_DEADLINE_STRIDE,
          RenderStage::Cascade,
        )
        .map_err(Error::Render)?;
        if let Some(targets) = exports.get(&internal) {
          for target in targets {
            check_active_periodic(
              &mut deadline_counter,
              SHADOW_MAP_DEADLINE_STRIDE,
              RenderStage::Cascade,
            )
            .map_err(Error::Render)?;
            push_part_export(&mut mapped, &alias, target.clone());
          }
        }
      }
      exports = mapped;
    }

    map.insert_host_exports(host_id, exports);
  }

  Ok(map)
}

pub(crate) const COMPAT_IMG_SRC_DATA_ATTR_CANDIDATES: [&str; 10] = [
  "data-gl-src",
  "data-src",
  "data-lazy-src",
  "data-original",
  "data-original-src",
  "data-url",
  "data-actualsrc",
  "data-img-src",
  "data-hires",
  "data-src-retina",
];

pub(crate) const COMPAT_IMG_SRCSET_DATA_ATTR_CANDIDATES: [&str; 6] = [
  "data-gl-srcset",
  "data-srcset",
  "data-lazy-srcset",
  "data-original-srcset",
  "data-original-set",
  "data-actualsrcset",
];

pub(crate) const COMPAT_SOURCE_SRCSET_DATA_ATTR_CANDIDATES: [&str; 6] = [
  "data-srcset",
  "data-lazy-srcset",
  "data-gl-srcset",
  "data-original-srcset",
  "data-original-set",
  "data-actualsrcset",
];

pub(crate) const COMPAT_SIZES_DATA_ATTR_CANDIDATES: [&str; 1] = ["data-sizes"];

pub(crate) const COMPAT_VIDEO_SRC_DATA_ATTR_CANDIDATES: [&str; 5] = [
  "data-video-src",
  "data-video-url",
  "data-src",
  "data-src-url",
  "data-url",
];

pub(crate) const COMPAT_AUDIO_SRC_DATA_ATTR_CANDIDATES: [&str; 4] =
  ["data-audio-src", "data-audio-url", "data-src", "data-url"];

pub(crate) const COMPAT_VIDEO_POSTER_DATA_ATTR_CANDIDATES: [&str; 5] = [
  "data-poster",
  "data-poster-url",
  "data-posterimage",
  "data-poster-image",
  "data-poster-image-override",
];

pub(crate) fn img_src_is_placeholder(value: &str) -> bool {
  fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
      .as_bytes()
      .get(..prefix.len())
      .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
  }

  fn trim_ascii_whitespace(value: &str) -> &str {
    value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
  }

  // `<img src>` is stripped of leading/trailing ASCII whitespace, but not all Unicode whitespace
  // (e.g. NBSP). Use an explicit ASCII trim so placeholder detection does not incorrectly treat
  // non-ASCII whitespace as empty.
  let value = trim_ascii_whitespace(value);
  if value.is_empty() {
    return true;
  }
  if value.starts_with('#') {
    return true;
  }
  if starts_with_ignore_ascii_case(value, "about:blank") {
    const PREFIX: &str = "about:blank";
    if matches!(
      value.as_bytes().get(PREFIX.len()),
      None | Some(b'#') | Some(b'?')
    ) {
      return true;
    }
  }
  if starts_with_ignore_ascii_case(value, "javascript:")
    || starts_with_ignore_ascii_case(value, "vbscript:")
    || starts_with_ignore_ascii_case(value, "mailto:")
  {
    return true;
  }

  // Treat the common "1x1 transparent GIF" data URLs used as placeholders for lazy-loaded images
  // as empty. These are typically replaced by client-side bootstrap JS with the real image URL.
  if !value
    .get(.."data:".len())
    .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
    .unwrap_or(false)
  {
    return false;
  }

  let rest = &value["data:".len()..];
  let Some((metadata, payload)) = rest.split_once(',') else {
    return false;
  };

  let mut parts = metadata.split(';');
  let mediatype = trim_ascii_whitespace(parts.next().unwrap_or(""));
  if !mediatype.eq_ignore_ascii_case("image/gif") {
    return false;
  }
  let is_base64 = parts.any(|part| trim_ascii_whitespace(part).eq_ignore_ascii_case("base64"));
  if !is_base64 {
    return false;
  }

  let payload = trim_ascii_whitespace(payload);
  if payload.is_empty() {
    return true;
  }
  // Avoid decoding unusually large data URLs; placeholders are tiny and should decode quickly.
  if payload.len() > 512 {
    return false;
  }

  let Ok(resource) = crate::resource::decode_data_url(value) else {
    return false;
  };
  let decoded = resource.bytes;

  if decoded.len() < 10 {
    return false;
  }

  if &decoded[..6] != b"GIF87a" && &decoded[..6] != b"GIF89a" {
    return false;
  }

  let width = u16::from_le_bytes([decoded[6], decoded[7]]);
  let height = u16::from_le_bytes([decoded[8], decoded[9]]);
  width == 1 && height == 1
}

/// Optional DOM compatibility tweaks applied after HTML parsing.
///
/// Some documents bootstrap by marking the root with `no-js` and replacing it with a
/// `js-enabled` class once scripts execute. Others toggle visibility gates like
/// `jsl10n-visible` after client-side localization. Since we do not run author scripts,
/// mirror those initializations so content that relies on the class flip (e.g., initial
/// opacity) is visible in static renders.
pub(crate) fn apply_dom_compatibility_mutations(
  node: &mut DomNode,
  deadline_counter: &mut usize,
) -> Result<()> {
  fn trim_ascii_whitespace(value: &str) -> &str {
    value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
  }

  fn first_non_empty_attr(attrs: &[(String, String)], names: &[&str]) -> Option<String> {
    for &name in names {
      if let Some((_, value)) = attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
        if !trim_ascii_whitespace(value).is_empty() {
          return Some(value.clone());
        }
      }
    }
    None
  }

  fn looks_like_url(value: &str) -> bool {
    let value = trim_ascii_whitespace(value);
    if value.is_empty() {
      return false;
    }
    if value.contains("://") || value.starts_with('/') {
      return true;
    }
    if value.starts_with("data:") || value.starts_with("blob:") {
      return true;
    }
    value.contains('.')
  }

  fn url_from_jsonish(value: &str) -> Option<String> {
    fn extract(value: &serde_json::Value) -> Option<String> {
      match value {
        serde_json::Value::String(s) => {
          let trimmed = trim_ascii_whitespace(s);
          if trimmed.is_empty() || !looks_like_url(trimmed) {
            return None;
          }
          Some(trimmed.to_string())
        }
        serde_json::Value::Array(values) => values.iter().find_map(extract),
        serde_json::Value::Object(map) => {
          const PRIORITY_KEYS: [&str; 7] = [
            "url",
            "src",
            "poster",
            "href",
            "poster_url",
            "posterUrl",
            "imageUrl",
          ];
          for key in PRIORITY_KEYS {
            if let Some(value) = map.get(key) {
              if let Some(url) = extract(value) {
                return Some(url);
              }
            }
          }
          map.values().find_map(extract)
        }
        _ => None,
      }
    }

    let value = trim_ascii_whitespace(value);
    if value.is_empty() {
      return None;
    }
    let first = value.chars().next()?;
    if first != '{' && first != '[' && first != '"' {
      return None;
    }
    let parsed = serde_json::from_str::<serde_json::Value>(value).ok()?;
    extract(&parsed)
  }

  fn first_non_empty_url_attr(attrs: &[(String, String)], names: &[&str]) -> Option<String> {
    for &name in names {
      if let Some((_, value)) = attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
        let trimmed = trim_ascii_whitespace(value);
        if trimmed.is_empty() {
          continue;
        }
        if let Some(url) = url_from_jsonish(trimmed) {
          return Some(url);
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') || trimmed.starts_with('"') {
          continue;
        }
        return Some(trimmed.to_string());
      }
    }
    None
  }

  fn url_looks_like_mp4(url: &str) -> bool {
    let url = trim_ascii_whitespace(url);
    if url.is_empty() {
      return false;
    }
    let lower = url.to_ascii_lowercase();
    let stem = lower.split(['?', '#']).next().unwrap_or(lower.as_str());
    stem.ends_with(".mp4")
  }

  fn video_url_from_urls_list(value: &str) -> Option<String> {
    let value = trim_ascii_whitespace(value);
    if value.is_empty() {
      return None;
    }
    if let Some(url) = url_from_jsonish(value) {
      return Some(url);
    }

    let mut first: Option<&str> = None;
    let mut mp4: Option<&str> = None;
    for part in value.split(',') {
      let part = trim_ascii_whitespace(part);
      if part.is_empty() {
        continue;
      }
      if first.is_none() {
        first = Some(part);
      }
      if mp4.is_none() && url_looks_like_mp4(part) {
        mp4 = Some(part);
      }
    }
    mp4.or(first).map(|url| url.to_string())
  }

  fn first_non_empty_video_src_candidate(attrs: &[(String, String)]) -> Option<String> {
    if let Some((_, urls)) = attrs
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case("data-video-urls"))
    {
      if let Some(url) = video_url_from_urls_list(urls) {
        return Some(url);
      }
    }
    first_non_empty_url_attr(attrs, &COMPAT_VIDEO_SRC_DATA_ATTR_CANDIDATES)
  }

  let mut stack: Vec<*mut DomNode> = Vec::new();
  stack.push(node as *mut DomNode);

  while let Some(ptr) = stack.pop() {
    check_active_periodic(
      deadline_counter,
      DOM_PARSE_NODE_DEADLINE_STRIDE,
      RenderStage::DomParse,
    )?;

    // Safety: we only push pointers to nodes owned by `node` (root) and never mutate a node's
    // `children` vec while any of its child pointers are in the stack, so raw pointers remain
    // valid for the duration of this traversal.
    let node = unsafe { &mut *ptr };
    let mut wrapper_video_urls: Option<String> = None;
    let mut wrapper_poster_url: Option<String> = None;

    if let DomNodeType::Element {
      tag_name,
      attributes,
      ..
    } = &mut node.node_type
    {
      let mut classes: Vec<String> = attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("class"))
        .map(|(_, v)| v.split_ascii_whitespace().map(|s| s.to_string()).collect())
        .unwrap_or_default();
      let mut changed = false;

      if tag_name.eq_ignore_ascii_case("html") {
        if classes.iter().any(|c| c == "no-js") {
          classes.retain(|c| c != "no-js");
          if !classes.iter().any(|c| c == "js-enabled") {
            classes.push("js-enabled".to_string());
          }
          changed = true;
        }
      }

      if tag_name.eq_ignore_ascii_case("html") || tag_name.eq_ignore_ascii_case("body") {
        if !classes.iter().any(|c| c == "jsl10n-visible") {
          classes.push("jsl10n-visible".to_string());
          changed = true;
        }
      }

      if changed {
        let class_value = classes.join(" ");
        if let Some((_, value)) = attributes
          .iter_mut()
          .find(|(k, _)| k.eq_ignore_ascii_case("class"))
        {
          *value = class_value;
        } else {
          attributes.push(("class".to_string(), class_value));
        }
      }

      if tag_name.eq_ignore_ascii_case("img") {
        // Some pages stash image URLs in `data-*` attributes (common for JS-driven lazy loading)
        // and rely on their bootstrap JS to populate `src`/`srcset`/`sizes`. When we don't execute
        // scripts, CSS like `img:not([src]):not([srcset]) { visibility: hidden }` can permanently
        // suppress the image.
        //
        // Compatibility mode mirrors this common bootstrap step by copying the first non-empty URL
        // from the following attributes, in priority order:
        //
        // - `src` ← `data-gl-src`, `data-src`, `data-lazy-src`, `data-original`, `data-original-src`,
        //   `data-url`, `data-actualsrc`, `data-img-src`, `data-hires`, `data-src-retina`
        // - `srcset` ← `data-gl-srcset`, `data-srcset`, `data-lazy-srcset`, `data-original-srcset`,
        //   `data-original-set`, `data-actualsrcset`
        // - `sizes` ← `data-sizes`
        //
        // Never override a non-empty authored attribute (except for known `src` placeholders like
        // `about:blank`, `#`, or 1×1 transparent GIF data URLs).

        let src_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("src"));
        let srcset_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("srcset"));
        let sizes_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("sizes"));

        let needs_src = match src_idx {
          Some(idx) => img_src_is_placeholder(&attributes[idx].1),
          None => true,
        };

        if needs_src {
          if let Some(candidate) =
            first_non_empty_attr(attributes, &COMPAT_IMG_SRC_DATA_ATTR_CANDIDATES)
          {
            match src_idx {
              Some(idx) => {
                if img_src_is_placeholder(&attributes[idx].1) {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("src".to_string(), candidate));
              }
            }
          }
        }

        let needs_srcset = match srcset_idx {
          Some(idx) => trim_ascii_whitespace(&attributes[idx].1).is_empty(),
          None => true,
        };
        if needs_srcset {
          if let Some(candidate) =
            first_non_empty_attr(attributes, &COMPAT_IMG_SRCSET_DATA_ATTR_CANDIDATES)
          {
            match srcset_idx {
              Some(idx) => {
                if trim_ascii_whitespace(&attributes[idx].1).is_empty() {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("srcset".to_string(), candidate));
              }
            }
          }
        }

        let needs_sizes = match sizes_idx {
          Some(idx) => trim_ascii_whitespace(&attributes[idx].1).is_empty(),
          None => true,
        };
        if needs_sizes {
          if let Some(candidate) =
            first_non_empty_attr(attributes, &COMPAT_SIZES_DATA_ATTR_CANDIDATES)
          {
            match sizes_idx {
              Some(idx) => {
                if trim_ascii_whitespace(&attributes[idx].1).is_empty() {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("sizes".to_string(), candidate));
              }
            }
          }
        }
      } else if tag_name.eq_ignore_ascii_case("source") {
        // Lazy-loaded `<picture>` sources often mirror the `<img>` pattern and delay populating
        // `srcset`/`sizes` until JS runs.

        let srcset_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("srcset"));
        let sizes_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("sizes"));

        let needs_srcset = match srcset_idx {
          Some(idx) => trim_ascii_whitespace(&attributes[idx].1).is_empty(),
          None => true,
        };
        if needs_srcset {
          if let Some(candidate) =
            first_non_empty_attr(attributes, &COMPAT_SOURCE_SRCSET_DATA_ATTR_CANDIDATES)
          {
            match srcset_idx {
              Some(idx) => {
                if trim_ascii_whitespace(&attributes[idx].1).is_empty() {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("srcset".to_string(), candidate));
              }
            }
          }
        }

        let needs_sizes = match sizes_idx {
          Some(idx) => trim_ascii_whitespace(&attributes[idx].1).is_empty(),
          None => true,
        };
        if needs_sizes {
          if let Some(candidate) =
            first_non_empty_attr(attributes, &COMPAT_SIZES_DATA_ATTR_CANDIDATES)
          {
            match sizes_idx {
              Some(idx) => {
                if trim_ascii_whitespace(&attributes[idx].1).is_empty() {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("sizes".to_string(), candidate));
              }
            }
          }
        }
      } else if tag_name.eq_ignore_ascii_case("iframe") {
        // Lazy iframe embeds often store the real URL in `data-src` until JS runs.
        let src_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("src"));
        let needs_src = match src_idx {
          Some(idx) => img_src_is_placeholder(&attributes[idx].1),
          None => true,
        };
        if needs_src {
          if let Some(candidate) = first_non_empty_attr(attributes, &["data-src"]) {
            match src_idx {
              Some(idx) => {
                if img_src_is_placeholder(&attributes[idx].1) {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("src".to_string(), candidate));
              }
            }
          }
        }
      } else if tag_name.eq_ignore_ascii_case("video") {
        let src_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("src"));
        let poster_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("poster"));

        let needs_src = match src_idx {
          Some(idx) => img_src_is_placeholder(&attributes[idx].1),
          None => true,
        };
        if needs_src {
          if let Some(candidate) = first_non_empty_video_src_candidate(attributes) {
            match src_idx {
              Some(idx) => {
                if img_src_is_placeholder(&attributes[idx].1) {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("src".to_string(), candidate));
              }
            }
          }
        }

        let needs_poster = match poster_idx {
          Some(idx) => img_src_is_placeholder(&attributes[idx].1),
          None => true,
        };
        if needs_poster {
          if let Some(candidate) =
            first_non_empty_url_attr(attributes, &COMPAT_VIDEO_POSTER_DATA_ATTR_CANDIDATES)
          {
            match poster_idx {
              Some(idx) => {
                if img_src_is_placeholder(&attributes[idx].1) {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("poster".to_string(), candidate));
              }
            }
          }
        }
      } else if tag_name.eq_ignore_ascii_case("audio") {
        let src_idx = attributes
          .iter()
          .position(|(name, _)| name.eq_ignore_ascii_case("src"));
        let needs_src = match src_idx {
          Some(idx) => img_src_is_placeholder(&attributes[idx].1),
          None => true,
        };
        if needs_src {
          if let Some(candidate) =
            first_non_empty_url_attr(attributes, &COMPAT_AUDIO_SRC_DATA_ATTR_CANDIDATES)
          {
            match src_idx {
              Some(idx) => {
                if img_src_is_placeholder(&attributes[idx].1) {
                  attributes[idx].1 = candidate;
                }
              }
              None => {
                attributes.push(("src".to_string(), candidate));
              }
            }
          }
        }
      }

      if !tag_name.eq_ignore_ascii_case("video") {
        wrapper_video_urls = first_non_empty_attr(attributes, &["data-video-urls"]);
        wrapper_poster_url = first_non_empty_attr(attributes, &["data-poster-url"]);
      }
    }

    if wrapper_video_urls.is_some() || wrapper_poster_url.is_some() {
      let wrapper_src = wrapper_video_urls
        .as_deref()
        .and_then(video_url_from_urls_list);
      let wrapper_poster = wrapper_poster_url.as_deref().and_then(|value| {
        let trimmed = trim_ascii_whitespace(value);
        if trimmed.is_empty() {
          return None;
        }
        if let Some(url) = url_from_jsonish(trimmed) {
          return Some(url);
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') || trimmed.starts_with('"') {
          return None;
        }
        Some(trimmed.to_string())
      });

      if wrapper_src.is_some() || wrapper_poster.is_some() {
        let mut descendant_stack: Vec<*mut DomNode> = Vec::new();
        let len = node.children.len();
        let children_ptr = node.children.as_mut_ptr();
        for idx in (0..len).rev() {
          descendant_stack.push(unsafe { children_ptr.add(idx) });
        }

        while let Some(ptr) = descendant_stack.pop() {
          check_active_periodic(
            deadline_counter,
            DOM_PARSE_NODE_DEADLINE_STRIDE,
            RenderStage::DomParse,
          )?;

          // Safety: descendant pointers are stable because we never mutate any `children` vectors.
          let current = unsafe { &mut *ptr };

          let mut is_video = false;
          if let DomNodeType::Element {
            tag_name,
            attributes,
            ..
          } = &mut current.node_type
          {
            if tag_name.eq_ignore_ascii_case("video") {
              is_video = true;

              if let Some(candidate) = &wrapper_src {
                let src_idx = attributes
                  .iter()
                  .position(|(name, _)| name.eq_ignore_ascii_case("src"));
                let needs_src = match src_idx {
                  Some(idx) => img_src_is_placeholder(&attributes[idx].1),
                  None => true,
                };
                if needs_src {
                  match src_idx {
                    Some(idx) => {
                      if img_src_is_placeholder(&attributes[idx].1) {
                        attributes[idx].1 = candidate.clone();
                      }
                    }
                    None => {
                      attributes.push(("src".to_string(), candidate.clone()));
                    }
                  }
                }
              }

              if let Some(candidate) = &wrapper_poster {
                let poster_idx = attributes
                  .iter()
                  .position(|(name, _)| name.eq_ignore_ascii_case("poster"));
                let needs_poster = match poster_idx {
                  Some(idx) => img_src_is_placeholder(&attributes[idx].1),
                  None => true,
                };
                if needs_poster {
                  match poster_idx {
                    Some(idx) => {
                      if img_src_is_placeholder(&attributes[idx].1) {
                        attributes[idx].1 = candidate.clone();
                      }
                    }
                    None => {
                      attributes.push(("poster".to_string(), candidate.clone()));
                    }
                  }
                }
              }
            }
          }

          if is_video {
            break;
          }

          let len = current.children.len();
          let children_ptr = current.children.as_mut_ptr();
          for idx in (0..len).rev() {
            descendant_stack.push(unsafe { children_ptr.add(idx) });
          }
        }
      }
    }

    let len = node.children.len();
    let children_ptr = node.children.as_mut_ptr();
    for idx in (0..len).rev() {
      // Safety: `children_ptr` came from `node.children` and the vector is not mutated until this
      // node is popped again (which will not happen), so these pointers remain valid.
      stack.push(unsafe { children_ptr.add(idx) });
    }
  }

  Ok(())
}

fn convert_handle_to_node(
  handle: &Handle,
  document_quirks_mode: QuirksMode,
  deadline_counter: &mut usize,
) -> Result<Option<DomNode>> {
  fn node_type_for_handle(
    handle: &Handle,
    document_quirks_mode: QuirksMode,
  ) -> Option<DomNodeType> {
    match &handle.data {
      NodeData::Document => Some(DomNodeType::Document {
        quirks_mode: document_quirks_mode,
      }),
      NodeData::Element { name, attrs, .. } => {
        let namespace = if name.ns.as_ref() == HTML_NAMESPACE {
          String::new()
        } else {
          name.ns.to_string()
        };
        let attrs_ref = attrs.borrow();
        let mut attributes = Vec::with_capacity(attrs_ref.len());
        for attr in attrs_ref.iter() {
          attributes.push((attr.name.local.to_string(), attr.value.to_string()));
        }

        let is_html_slot = name.local.as_ref().eq_ignore_ascii_case("slot")
          && (namespace.is_empty() || namespace == HTML_NAMESPACE);

        if is_html_slot {
          Some(DomNodeType::Slot {
            namespace,
            attributes,
            assigned: false,
          })
        } else {
          let tag_name = name.local.to_string();
          Some(DomNodeType::Element {
            tag_name,
            namespace,
            attributes,
          })
        }
      }
      NodeData::Text { contents } => Some(DomNodeType::Text {
        content: contents.borrow().to_string(),
      }),
      _ => None,
    }
  }

  fn children_info(handle: &Handle) -> (bool, Option<Handle>, usize) {
    match &handle.data {
      NodeData::Document => (false, None, handle.children.borrow().len()),
      NodeData::Element {
        name,
        template_contents,
        ..
      } => {
        if name.local.as_ref().eq_ignore_ascii_case("template") {
          let borrowed = template_contents.borrow();
          match &*borrowed {
            Some(content) => (true, Some(content.clone()), content.children.borrow().len()),
            None => (true, None, 0),
          }
        } else {
          (false, None, handle.children.borrow().len())
        }
      }
      _ => (false, None, 0),
    }
  }

  struct Frame {
    handle: Handle,
    dst: *mut DomNode,
    next_child: usize,
    children_len: usize,
    use_template_contents: bool,
    template_content: Option<Handle>,
  }

  check_active_periodic(
    deadline_counter,
    DOM_PARSE_NODE_DEADLINE_STRIDE,
    RenderStage::DomParse,
  )?;

  let Some(node_type) = node_type_for_handle(handle, document_quirks_mode) else {
    return Ok(None);
  };

  let (use_template_contents, template_content, children_len) = children_info(handle);
  let mut root = DomNode {
    node_type,
    children: Vec::with_capacity(children_len),
  };

  let mut stack = vec![Frame {
    handle: handle.clone(),
    dst: &mut root as *mut DomNode,
    next_child: 0,
    children_len,
    use_template_contents,
    template_content,
  }];

  while let Some(mut frame) = stack.pop() {
    // Safety: destination nodes are owned by `root` and its descendants, and we never mutate a
    // node's children while a frame borrowing that node is active. This keeps raw pointers stable
    // for the duration of the DFS conversion.
    let dst = unsafe { &mut *frame.dst };

    if frame.next_child < frame.children_len {
      let child_handle = if frame.use_template_contents {
        let Some(content) = frame.template_content.as_ref() else {
          frame.next_child = frame.children_len;
          stack.push(frame);
          continue;
        };
        let handle = content.children.borrow().get(frame.next_child).cloned();
        handle.ok_or_else(|| {
          Error::Parse(ParseError::InvalidHtml {
            message: "DOM conversion encountered an out-of-bounds template content child".to_string(),
            line: 0,
          })
        })?
      } else {
        let handle = frame.handle.children.borrow().get(frame.next_child).cloned();
        handle.ok_or_else(|| {
          Error::Parse(ParseError::InvalidHtml {
            message: "DOM conversion encountered an out-of-bounds child".to_string(),
            line: 0,
          })
        })?
      };
      frame.next_child += 1;
      stack.push(frame);

      check_active_periodic(
        deadline_counter,
        DOM_PARSE_NODE_DEADLINE_STRIDE,
        RenderStage::DomParse,
      )?;

      let Some(child_type) = node_type_for_handle(&child_handle, document_quirks_mode) else {
        continue;
      };

      let (child_use_template, child_template_content, child_len) = children_info(&child_handle);
      dst.children.push(DomNode {
        node_type: child_type,
        children: Vec::with_capacity(child_len),
      });
      let child_dst = dst
        .children
        .last_mut()
        .map(|node| node as *mut DomNode)
        .ok_or_else(|| {
          Error::Parse(ParseError::InvalidHtml {
            message: "DOM conversion failed to append a child node".to_string(),
            line: 0,
          })
        })?;

      stack.push(Frame {
        handle: child_handle,
        dst: child_dst,
        next_child: 0,
        children_len: child_len,
        use_template_contents: child_use_template,
        template_content: child_template_content,
      });

      continue;
    }

    // HTML <wbr> elements represent optional break opportunities. Synthesize a zero-width break
    // text node so line breaking can consider the opportunity while still allowing the element to
    // be styled/selected.
    if let DomNodeType::Element { tag_name, .. } = &dst.node_type {
      if tag_name.eq_ignore_ascii_case("wbr") {
        dst.children.push(DomNode {
          node_type: DomNodeType::Text {
            content: "\u{200B}".to_string(),
          },
          children: Vec::new(),
        });
      }
    }
  }

  Ok(Some(root))
}

impl DomNode {
  pub(crate) fn clone_without_children(&self) -> DomNode {
    DomNode {
      node_type: self.node_type.clone(),
      children: Vec::new(),
    }
  }

  /// Clone this node without cloning its children.
  ///
  /// This is used when building the styled tree: `StyledNode.children` represents the tree
  /// structure, so cloning descendant DOM nodes into every `StyledNode.node` is redundant and
  /// extremely expensive on large documents.
  pub fn clone_shallow(&self) -> Self {
    self.clone_without_children()
  }

  pub fn get_attribute_ref(&self, name: &str) -> Option<&str> {
    match &self.node_type {
      DomNodeType::Element { attributes, .. } => attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str()),
      DomNodeType::Slot { attributes, .. } => attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str()),
      _ => None,
    }
  }

  pub fn get_attribute(&self, name: &str) -> Option<String> {
    self.get_attribute_ref(name).map(|v| v.to_string())
  }

  /// Set (or replace) an attribute on element/slot nodes.
  ///
  /// Attribute name comparisons are ASCII case-insensitive, matching [`DomNode::get_attribute_ref`].
  ///
  /// This is a no-op on non-element nodes.
  pub fn set_attribute(&mut self, name: &str, value: &str) {
    match &mut self.node_type {
      DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
        set_attr(attributes, name, value);
      }
      _ => {}
    }
  }

  /// Remove an attribute from element/slot nodes.
  ///
  /// Attribute name comparisons are ASCII case-insensitive, matching [`DomNode::get_attribute_ref`].
  ///
  /// This is a no-op on non-element nodes.
  pub fn remove_attribute(&mut self, name: &str) {
    match &mut self.node_type {
      DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
        remove_attr(attributes, name);
      }
      _ => {}
    }
  }

  /// Toggle a boolean attribute on element/slot nodes.
  ///
  /// When `enabled` is true, sets `name=""`. When false, removes the attribute.
  ///
  /// This is a no-op on non-element nodes.
  pub fn toggle_bool_attribute(&mut self, name: &str, enabled: bool) {
    if enabled {
      self.set_attribute(name, "");
    } else {
      self.remove_attribute(name);
    }
  }

  pub fn tag_name(&self) -> Option<&str> {
    match &self.node_type {
      DomNodeType::Element { tag_name, .. } => Some(tag_name),
      DomNodeType::Slot { .. } => Some("slot"),
      _ => None,
    }
  }

  pub fn is_template_element(&self) -> bool {
    matches!(
      self.tag_name(),
      Some(tag) if tag.eq_ignore_ascii_case("template")
    )
  }

  pub(crate) fn traversal_children(&self) -> &[DomNode] {
    if self.is_template_element() {
      &[]
    } else {
      &self.children
    }
  }

  pub fn namespace(&self) -> Option<&str> {
    match &self.node_type {
      DomNodeType::Element { namespace, .. } => Some(namespace),
      DomNodeType::Slot { namespace, .. } => Some(namespace),
      _ => None,
    }
  }

  pub fn is_html_template_element(&self) -> bool {
    self
      .tag_name()
      .map(|tag| tag.eq_ignore_ascii_case("template"))
      .unwrap_or(false)
      && matches!(self.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
  }

  /// Returns true if this node is a `<template>` element whose contents should be treated as inert.
  ///
  /// `parse_html` promotes the first declarative shadow DOM template for each host into a
  /// `DomNodeType::ShadowRoot`, leaving any remaining `<template>` nodes (including unused
  /// `<template shadowroot=...>` siblings) in the light DOM. Those remaining templates behave as
  /// inert template contents for all post-parse traversals (CSS extraction, prefetch discovery,
  /// selector matching, etc).
  pub fn template_contents_are_inert(&self) -> bool {
    matches!(self.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("template"))
  }

  pub fn document_quirks_mode(&self) -> QuirksMode {
    if let DomNodeType::Document { quirks_mode } = &self.node_type {
      *quirks_mode
    } else {
      debug_assert!(
        matches!(self.node_type, DomNodeType::Document { .. }),
        "document_quirks_mode called on non-document node; defaulting to NoQuirks"
      );
      QuirksMode::NoQuirks
    }
  }

  pub fn is_shadow_host(&self) -> bool {
    matches!(
      self.node_type,
      DomNodeType::Element { .. } | DomNodeType::Slot { .. }
    ) && self
      .children
      .iter()
      .any(|c| matches!(c.node_type, DomNodeType::ShadowRoot { .. }))
  }

  pub fn attributes_iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
    let attrs: &[(String, String)] = match &self.node_type {
      DomNodeType::Element { attributes, .. } => attributes,
      DomNodeType::Slot { attributes, .. } => attributes,
      _ => &[],
    };
    attrs.iter().map(|(k, v)| (k.as_str(), v.as_str()))
  }

  pub fn is_element(&self) -> bool {
    matches!(
      self.node_type,
      DomNodeType::Element { .. } | DomNodeType::Slot { .. }
    )
  }

  pub fn is_text(&self) -> bool {
    matches!(self.node_type, DomNodeType::Text { .. })
  }

  pub fn text_content(&self) -> Option<&str> {
    match &self.node_type {
      DomNodeType::Text { content } => Some(content),
      _ => None,
    }
  }

  pub fn walk_tree<F>(&self, f: &mut F)
  where
    F: FnMut(&DomNode),
  {
    // Avoid recursion for extremely deep/degenerate DOM trees.
    let mut stack: Vec<&DomNode> = Vec::new();
    stack.push(self);

    while let Some(node) = stack.pop() {
      f(node);
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
  }

  /// Get element children (skip text nodes)
  pub fn element_children(&self) -> Vec<&DomNode> {
    self.children.iter().filter(|c| c.is_element()).collect()
  }

  /// Check if this element has a specific class
  pub fn has_class(&self, class: &str) -> bool {
    if let Some(class_attr) = self.get_attribute_ref("class") {
      class_attr.split_ascii_whitespace().any(|c| c == class)
    } else {
      false
    }
  }

  /// Check if this element has a specific ID
  pub fn has_id(&self, id: &str) -> bool {
    self.get_attribute_ref("id") == Some(id)
  }
}

fn is_ascii_whitespace_html(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}')
}

fn trim_ascii_whitespace_html(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html)
}

/// Parse an `exportparts` attribute value into (internal, exported) name pairs.
///
/// Entries are comma-separated. A missing alias (i.e. `ident` without a `:`) is treated as an
/// identity mapping. Invalid mappings like `ident:` (missing the outer ident) are ignored.
pub(crate) fn parse_exportparts(value: &str) -> Vec<(String, String)> {
  let mut mappings = Vec::new();
  for entry in value.split(',') {
    let entry = trim_ascii_whitespace_html(entry);
    if entry.is_empty() {
      continue;
    }

    // `exportparts` uses a colon separator between internal and exported names, but the internal
    // token can itself start with a `::pseudo-element` name. Handle that by ignoring the leading
    // `::` prefix when locating the separator.
    //
    // Spec example:
    //   exportparts="::before : preceding-text"
    //
    // A naive `splitn(':')` would treat the first colon of the `::` prefix as the separator and
    // drop the mapping entirely.
    let split_idx = if entry.starts_with("::") {
      entry[2..].find(':').map(|idx| idx + 2)
    } else {
      entry.find(':')
    };
    let (internal, exported) = match split_idx {
      Some(idx) => {
        let internal = trim_ascii_whitespace_html(&entry[..idx]);
        let exported = trim_ascii_whitespace_html(&entry[idx + 1..]);
        (internal, Some(exported))
      }
      None => (trim_ascii_whitespace_html(entry), None),
    };
    if internal.is_empty() {
      continue;
    }

    match exported {
      Some(alias) if !alias.is_empty() => {
        mappings.push((internal.to_string(), alias.to_string()));
      }
      // Per CSS Shadow Parts, `ident:` is invalid and ignored (rather than treated as `ident`).
      Some(_) => {}
      None => {
        mappings.push((internal.to_string(), internal.to_string()));
      }
    }
  }
  mappings
}

pub(crate) fn exportparts_exportable_pseudo(internal: &str) -> Option<PseudoElement> {
  // Per CSS Shadow Parts, `exportparts="::pseudo: name"` only forwards fully-styleable
  // pseudo-elements. Reject restricted pseudos like `::marker` and `::placeholder`.
  if internal.eq_ignore_ascii_case("::before") {
    Some(PseudoElement::Before)
  } else if internal.eq_ignore_ascii_case("::after") {
    Some(PseudoElement::After)
  } else if internal.eq_ignore_ascii_case("::backdrop")
    || internal.eq_ignore_ascii_case("::-webkit-backdrop")
    || internal.eq_ignore_ascii_case("::-ms-backdrop")
  {
    Some(PseudoElement::Backdrop)
  } else if internal.eq_ignore_ascii_case("::file-selector-button")
    || internal.eq_ignore_ascii_case("::-webkit-file-upload-button")
  {
    Some(PseudoElement::FileSelectorButton)
  } else if internal.eq_ignore_ascii_case("::slider-thumb")
    || internal.eq_ignore_ascii_case("::-webkit-slider-thumb")
    || internal.eq_ignore_ascii_case("::-moz-range-thumb")
    || internal.eq_ignore_ascii_case("::-ms-thumb")
  {
    Some(PseudoElement::SliderThumb)
  } else if internal.eq_ignore_ascii_case("::slider-track")
    || internal.eq_ignore_ascii_case("::-webkit-slider-runnable-track")
    || internal.eq_ignore_ascii_case("::-moz-range-track")
    || internal.eq_ignore_ascii_case("::-ms-track")
  {
    Some(PseudoElement::SliderTrack)
  } else {
    None
  }
}

fn parse_finite_number(value: &str) -> Option<f64> {
  trim_ascii_whitespace_html(value)
    .parse::<f64>()
    .ok()
    .filter(|v| v.is_finite())
}

pub(crate) fn format_number(mut value: f64) -> String {
  if value == -0.0 {
    value = 0.0;
  }
  let mut s = value.to_string();
  if s.contains('.') {
    while s.ends_with('0') {
      s.pop();
    }
    if s.ends_with('.') {
      s.pop();
    }
  }
  s
}

pub(crate) fn input_range_bounds(node: &DomNode) -> Option<(f64, f64)> {
  if !matches!(node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("input")) {
    return None;
  }

  let input_type = node.get_attribute_ref("type");
  if !matches!(input_type, Some(t) if t.eq_ignore_ascii_case("range")) {
    return None;
  }

  let min = node
    .get_attribute_ref("min")
    .and_then(parse_finite_number)
    .unwrap_or(0.0);
  let max = node
    .get_attribute_ref("max")
    .and_then(parse_finite_number)
    .unwrap_or(100.0);

  // The HTML value sanitization algorithm collapses invalid ranges. When max < min, treat max as
  // min so downstream clamping produces a usable value instead of marking the control invalid.
  let clamped_max = if max < min { min } else { max };
  Some((min, clamped_max))
}

pub(crate) fn input_range_value(node: &DomNode) -> Option<f64> {
  let (min, max) = input_range_bounds(node)?;

  let resolved = node
    .get_attribute_ref("value")
    .and_then(parse_finite_number)
    .unwrap_or_else(|| (min + max) / 2.0);

  let clamped = resolved.clamp(min, max);

  let step_attr = node.get_attribute_ref("step");
  if matches!(
    step_attr,
    Some(step) if trim_ascii_whitespace_html(step).eq_ignore_ascii_case("any")
  ) {
    return Some(clamped);
  }

  let step = step_attr
    .and_then(parse_finite_number)
    .filter(|step| *step > 0.0)
    .unwrap_or(1.0);

  // The allowed value step base for range inputs is the minimum value (defaulting to zero).
  let step_base = min;
  let steps_to_value = ((clamped - step_base) / step).round();
  let mut aligned = step_base + steps_to_value * step;

  let max_aligned = step_base + ((max - step_base) / step).floor() * step;
  if aligned > max_aligned {
    aligned = max_aligned;
  }
  if aligned < step_base {
    aligned = step_base;
  }

  Some(aligned.clamp(min, max))
}

fn parse_simple_color_hex(value: &str) -> Option<(u8, u8, u8)> {
  // https://html.spec.whatwg.org/multipage/input.html#simple-colour
  //
  // A "simple color" is exactly 7 code points: '#' followed by 6 ASCII hex digits. Unlike some
  // other HTML attributes (e.g. `bgcolor`), this is not a general CSS color syntax.
  if value.len() != 7 || !value.starts_with('#') {
    return None;
  }
  let hex = &value[1..];
  if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
    return None;
  }
  let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
  let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
  let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
  Some((r, g, b))
}

pub(crate) fn input_color_value_string(node: &DomNode) -> Option<String> {
  if !matches!(node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("input")) {
    return None;
  }

  let input_type = node.get_attribute_ref("type");
  if !matches!(input_type, Some(t) if t.eq_ignore_ascii_case("color")) {
    return None;
  }

  // Color input values are sanitized to a simple color, defaulting to black.
  // (This is why `required` does not apply to them.)
  let raw = node.get_attribute_ref("value").unwrap_or("");
  let (r, g, b) = parse_simple_color_hex(raw).unwrap_or((0, 0, 0));
  Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

fn sanitize_input_value_string(
  node: &DomNode,
  expected_type: &str,
  is_valid: impl FnOnce(&str) -> bool,
) -> Option<String> {
  if !matches!(node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("input")) {
    return None;
  }

  let input_type = node.get_attribute_ref("type");
  if !matches!(input_type, Some(t) if t.eq_ignore_ascii_case(expected_type)) {
    return None;
  }

  let raw = node.get_attribute_ref("value").unwrap_or("");
  let trimmed = trim_ascii_whitespace_html(raw);
  if trimmed.is_empty() {
    return Some(String::new());
  }

  if is_valid(trimmed) {
    Some(trimmed.to_string())
  } else {
    Some(String::new())
  }
}

pub(crate) fn input_number_value_string(node: &DomNode) -> Option<String> {
  if !matches!(node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("input")) {
    return None;
  }

  let input_type = node.get_attribute_ref("type");
  if !matches!(input_type, Some(t) if t.eq_ignore_ascii_case("number")) {
    return None;
  }

  let raw = node.get_attribute_ref("value").unwrap_or("");
  let trimmed = trim_ascii_whitespace_html(raw);
  if trimmed.is_empty() {
    return Some(String::new());
  }

  // HTML number inputs sanitize invalid values to the empty string.
  if parse_finite_number(raw).is_some() {
    Some(trimmed.to_string())
  } else {
    Some(String::new())
  }
}

pub(crate) fn input_date_value_string(node: &DomNode) -> Option<String> {
  sanitize_input_value_string(node, "date", |value| {
    forms_validation::parse_date_value(value).is_some()
  })
}

pub(crate) fn input_time_value_string(node: &DomNode) -> Option<String> {
  sanitize_input_value_string(node, "time", |value| {
    forms_validation::parse_time_value(value).is_some()
  })
}

pub(crate) fn input_datetime_local_value_string(node: &DomNode) -> Option<String> {
  sanitize_input_value_string(node, "datetime-local", |value| {
    forms_validation::parse_datetime_local_value(value).is_some()
  })
}

pub(crate) fn input_month_value_string(node: &DomNode) -> Option<String> {
  sanitize_input_value_string(node, "month", |value| {
    forms_validation::parse_month_value(value).is_some()
  })
}

pub(crate) fn input_week_value_string(node: &DomNode) -> Option<String> {
  sanitize_input_value_string(node, "week", |value| {
    forms_validation::parse_week_value(value).is_some()
  })
}

/// Wrapper for DomNode that implements Element trait for selector matching
/// This wrapper carries context needed for matching (parent, siblings)
#[derive(Debug, Clone, Copy)]
pub struct ElementRef<'a> {
  pub node: &'a DomNode,
  pub node_id: usize,
  pub parent: Option<&'a DomNode>,
  all_ancestors: &'a [&'a DomNode],
  slot_map: Option<&'a crate::css::selectors::SlotAssignmentMap<'a>>,
  attr_cache: Option<&'a ElementAttrCache>,
}

impl<'a> ElementRef<'a> {
  pub fn new(node: &'a DomNode) -> Self {
    Self {
      node,
      node_id: 0,
      parent: None,
      all_ancestors: &[],
      slot_map: None,
      attr_cache: None,
    }
  }

  pub fn with_ancestors(node: &'a DomNode, ancestors: &'a [&'a DomNode]) -> Self {
    let parent = ancestors.last().copied();
    Self {
      node,
      node_id: 0,
      parent,
      all_ancestors: ancestors,
      slot_map: None,
      attr_cache: None,
    }
  }

  pub fn with_node_id(mut self, node_id: usize) -> Self {
    self.node_id = node_id;
    self
  }

  pub fn with_slot_map(
    mut self,
    slot_map: Option<&'a crate::css::selectors::SlotAssignmentMap<'a>>,
  ) -> Self {
    self.slot_map = slot_map;
    self
  }

  pub fn with_attr_cache(mut self, attr_cache: Option<&'a ElementAttrCache>) -> Self {
    self.attr_cache = attr_cache;
    self
  }

  fn visited_flag(&self) -> bool {
    self
      .node
      .get_attribute_ref("data-fastr-visited")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false)
  }

  fn active_flag(&self) -> bool {
    if self.inert_flag() {
      return false;
    }
    self
      .node
      .get_attribute_ref("data-fastr-active")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false)
  }

  fn hover_flag(&self) -> bool {
    if self.inert_flag() {
      return false;
    }
    self
      .node
      .get_attribute_ref("data-fastr-hover")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false)
  }

  fn node_is_inert(node: &DomNode) -> bool {
    matches!(
      node.node_type,
      DomNodeType::Element { .. } | DomNodeType::Slot { .. }
    ) && (node.get_attribute_ref("inert").is_some()
      || node
        .get_attribute_ref("data-fastr-inert")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false))
  }

  fn inert_flag(&self) -> bool {
    if Self::node_is_inert(self.node) {
      return true;
    }
    self
      .all_ancestors
      .iter()
      .any(|ancestor| Self::node_is_inert(ancestor))
  }

  fn node_focus_flag(node: &DomNode) -> bool {
    if Self::node_is_inert(node) {
      return false;
    }
    if let DomNodeType::Element { namespace, .. } = &node.node_type {
      if namespace == SVG_NAMESPACE {
        let focusable = node
          .get_attribute_ref("focusable")
          .map(|v| v.eq_ignore_ascii_case("true"))
          .unwrap_or(false);
        if !focusable {
          return false;
        }
      }
    } else {
      return false;
    }

    node
      .get_attribute_ref("data-fastr-focus")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false)
  }

  fn focus_flag(&self) -> bool {
    if self.inert_flag() {
      return false;
    }
    Self::node_focus_flag(self.node)
  }

  fn focus_visible_flag(&self) -> bool {
    if self.inert_flag() {
      return false;
    }
    if !Self::node_focus_flag(self.node) {
      return false;
    }

    self
      .node
      .get_attribute_ref("data-fastr-focus-visible")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false)
  }

  fn user_validity_flag(&self) -> bool {
    // HTML "user validity" is initially false and flips to true after a user interaction /
    // submission attempt. Since FastRender is a static renderer (no DOM events), we expose an
    // explicit opt-in hint to treat the control as user-validated:
    //
    // - `data-fastr-user-validity="true"` on the control itself, or
    // - `data-fastr-user-validity="true"` on its form owner.
    //
    // Without these hints, `:user-valid` / `:user-invalid` match nothing on a fresh document.
    if let Some(value) = self.node.get_attribute_ref("data-fastr-user-validity") {
      return value.eq_ignore_ascii_case("true");
    }

    let Some(form) = self.form_owner() else {
      return false;
    };
    form
      .get_attribute_ref("data-fastr-user-validity")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false)
  }

  fn push_assigned_slot_nodes<'b>(
    current: &'b DomNode,
    slot_map: Option<&SlotAssignmentMap<'b>>,
    visited: Option<&HashSet<usize>>,
    stack: &mut Vec<&'b DomNode>,
  ) -> bool {
    let Some(map) = slot_map else {
      return false;
    };
    if !matches!(current.node_type, DomNodeType::Slot { .. }) {
      return false;
    }
    let Some(slot_id) = map.slot_id(current) else {
      return false;
    };
    let Some(assigned_ids) = map.assigned_node_ids(slot_id) else {
      return false;
    };

    let mut pushed = false;
    for assigned_id in assigned_ids.iter().rev() {
      let Some(assigned) = map.node_for_id(*assigned_id) else {
        continue;
      };
      if let Some(seen) = visited {
        if let Some(id) = map.node_id(assigned) {
          if seen.contains(&id) {
            continue;
          }
        }
      }
      stack.push(assigned);
      pushed = true;
    }
    pushed
  }

  fn subtree_contains_focus(&self, slot_map: Option<&SlotAssignmentMap<'_>>) -> bool {
    if self.inert_flag() {
      return false;
    }
    Self::node_or_descendant_has_focus(self.node, slot_map)
  }

  fn node_or_descendant_has_focus(
    node: &DomNode,
    slot_map: Option<&SlotAssignmentMap<'_>>,
  ) -> bool {
    let mut stack: Vec<&DomNode> = vec![node];
    let mut visited = slot_map.is_some().then(HashSet::new);

    while let Some(current) = stack.pop() {
      if Self::node_is_inert(current) {
        continue;
      }
      if let (Some(map), Some(ref mut seen)) = (slot_map, visited.as_mut()) {
        if let Some(id) = map.node_id(current) {
          if !seen.insert(id) {
            continue;
          }
        }
      }
      if Self::node_focus_flag(current) {
        return true;
      }

      let assigned_children_pushed =
        Self::push_assigned_slot_nodes(current, slot_map, visited.as_ref(), &mut stack);

      if !assigned_children_pushed {
        for child in current.traversal_children().iter().rev() {
          stack.push(child);
        }
      }
    }

    false
  }

  fn subtree_has_content(node: &DomNode) -> bool {
    // Avoid recursion to prevent stack overflows in pathological trees (e.g., fuzzing or malformed
    // DOM inputs that violate our usual structural assumptions about where shadow roots/documents
    // can appear).
    let mut stack: Vec<&DomNode> = vec![node];
    while let Some(current) = stack.pop() {
      match &current.node_type {
        DomNodeType::Text { .. } => return true,
        DomNodeType::Element { .. } | DomNodeType::Slot { .. } => return true,
        DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. } => {
          for child in current.children.iter().rev() {
            stack.push(child);
          }
        }
      }
    }
    false
  }

  fn sibling_position(
    &self,
    context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
  ) -> Option<SiblingPosition> {
    let parent = self.parent?;
    if let Some(cache) = context.extra_data.sibling_cache {
      return cache.position(parent, self.node, context);
    }

    build_parent_sibling_list(parent, context)
      .and_then(|entry| entry.positions.get(&(self.node as *const DomNode)).copied())
  }

  /// Find index of this element among sibling elements and the total number of element siblings.
  fn element_index_and_len(
    &self,
    context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
  ) -> Option<(usize, usize)> {
    self
      .sibling_position(context)
      .map(|position| (position.index, position.len))
  }

  /// Find index of this element among siblings
  fn element_index(
    &self,
    context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
  ) -> Option<usize> {
    self.element_index_and_len(context).map(|(idx, _)| idx)
  }

  fn is_html_element(&self) -> bool {
    matches!(
      self.node.node_type,
      DomNodeType::Element { ref namespace, .. } | DomNodeType::Slot { ref namespace, .. }
        if namespace.is_empty() || namespace == HTML_NAMESPACE
    )
  }

  fn is_shadow_host(&self) -> bool {
    matches!(
      self.node.node_type,
      DomNodeType::Element { .. } | DomNodeType::Slot { .. }
    ) && self
      .node
      .children
      .iter()
      .any(|child| matches!(child.node_type, DomNodeType::ShadowRoot { .. }))
  }

  /// Position (index, total) among siblings filtered by a predicate.
  fn position_in_siblings<F>(
    &self,
    predicate: F,
    context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
  ) -> Option<(usize, usize)>
  where
    F: Fn(&DomNode) -> bool,
  {
    let parent = self.parent?;
    let element_children: Vec<*const DomNode> =
      if let Some(cache) = context.extra_data.sibling_cache {
        cache.ordered_children(parent, context)?
      } else {
        parent
          .children
          .iter()
          .filter(|c| c.is_element())
          .map(|c| c as *const DomNode)
          .collect()
      };

    let mut index = None;
    let mut len = 0usize;
    let mut deadline_counter = 0usize;
    for child_ptr in element_children {
      if let Err(err) = check_active_periodic(
        &mut deadline_counter,
        NTH_DEADLINE_STRIDE,
        RenderStage::Cascade,
      ) {
        context.extra_data.record_deadline_error(err);
        return None;
      }
      // Safety: DOM nodes are immutable during selector matching; pointers come from the DOM tree.
      let child = unsafe { &*child_ptr };
      if !predicate(child) {
        continue;
      }
      if ptr::eq(child, self.node) {
        index = Some(len);
      }
      len += 1;
    }
    index.map(|idx| (idx, len))
  }

  /// Position among siblings of the same element type (case-insensitive).
  fn position_in_type(
    &self,
    context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
  ) -> Option<(usize, usize)> {
    if self.node.tag_name().is_none() {
      return None;
    }
    self
      .sibling_position(context)
      .map(|position| (position.type_index, position.type_len))
  }

  fn populate_nth_index_cache_for_selectors(
    &self,
    selectors: &selectors::parser::SelectorList<FastRenderSelectorImpl>,
    is_from_end: bool,
    context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>,
  ) -> Option<i32> {
    let parent = self.parent?;
    let (entries, self_index) = context.nest(|context| {
      let mut entries: Vec<(OpaqueElement, i32)> = Vec::with_capacity(parent.children.len());
      let mut self_index: Option<i32> = None;
      let mut matching_index = 0i32;
      let mut deadline_counter = 0usize;

      let mut process_child =
        |child: &DomNode,
         context: &mut selectors::matching::MatchingContext<FastRenderSelectorImpl>|
         -> Option<()> {
          if let Err(err) = check_active_periodic(
            &mut deadline_counter,
            NTH_DEADLINE_STRIDE,
            RenderStage::Cascade,
          ) {
            context.extra_data.record_deadline_error(err);
            return None;
          }
          if !child.is_element() {
            return Some(());
          }
          let child_ref = ElementRef::with_ancestors(child, self.all_ancestors)
            .with_slot_map(self.slot_map)
            .with_attr_cache(self.attr_cache);
          let matches = selectors
            .slice()
            .iter()
            .any(|selector| matches_selector(selector, 0, None, &child_ref, context));
          if context.extra_data.deadline_error.is_some() {
            return None;
          }

          let idx = if matches {
            matching_index += 1;
            matching_index
          } else {
            0
          };
          entries.push((OpaqueElement::new(child), idx));

          if ptr::eq(child, self.node) {
            self_index = Some(idx);
          }
          Some(())
        };

      if is_from_end {
        for child in parent.children.iter().rev() {
          process_child(child, context)?;
        }
      } else {
        for child in parent.children.iter() {
          process_child(child, context)?;
        }
      }

      Some((entries, self_index))
    })?;

    let cache = context.nth_index_cache(false, is_from_end, selectors.slice());
    for (el, idx) in entries {
      cache.insert(el, idx);
    }

    #[cfg(test)]
    NTH_OF_CACHE_POPULATIONS.with(|counter| {
      counter.fetch_add(1, Ordering::Relaxed);
    });

    Some(self_index.unwrap_or(0))
  }

  /// Return the language of this element, inherited from ancestors if absent.
  fn language(&self) -> Option<Cow<'a, str>> {
    // Walk from self up through ancestors (closest first) for lang/xml:lang.
    if let Some(lang) = self.lang_attribute(self.node).filter(|l| !l.is_empty()) {
      let lang = normalize_language_tag_for_selector_matching(lang);
      if lang.is_empty() {
        return None;
      }
      return Some(lang);
    }

    for ancestor in self.all_ancestors.iter().rev() {
      if let Some(lang) = self.lang_attribute(ancestor).filter(|l| !l.is_empty()) {
        let lang = normalize_language_tag_for_selector_matching(lang);
        if lang.is_empty() {
          return None;
        }
        return Some(lang);
      }
    }
    None
  }

  fn lang_attribute(&self, node: &'a DomNode) -> Option<&'a str> {
    node
      .get_attribute_ref("lang")
      .or_else(|| node.get_attribute_ref("xml:lang"))
  }

  fn supports_disabled(&self) -> bool {
    if !self.is_html_element() {
      return false;
    }
    self.node.tag_name().is_some_and(|tag| {
      tag.eq_ignore_ascii_case("button")
        || tag.eq_ignore_ascii_case("input")
        || tag.eq_ignore_ascii_case("select")
        || tag.eq_ignore_ascii_case("textarea")
        || tag.eq_ignore_ascii_case("option")
        || tag.eq_ignore_ascii_case("optgroup")
        || tag.eq_ignore_ascii_case("fieldset")
    })
  }

  fn is_disabled(&self) -> bool {
    let Some(tag) = self.node.tag_name() else {
      return false;
    };

    if self.supports_disabled() && self.node.get_attribute_ref("disabled").is_some() {
      return true;
    }

    // Fieldset disabled state propagates to descendants except those inside the first legend.
    for (i, ancestor) in self.all_ancestors.iter().enumerate().rev() {
      if let Some(a_tag) = ancestor.tag_name() {
        if a_tag.eq_ignore_ascii_case("fieldset")
          && ancestor.get_attribute_ref("disabled").is_some()
        {
          // Find first legend child of this fieldset.
          let element_children = ancestor.element_children();
          let first_legend = element_children.iter().find(|child| {
            child
              .tag_name()
              .map(|t| t.eq_ignore_ascii_case("legend"))
              .unwrap_or(false)
          });

          if let Some(legend) = first_legend {
            // If we are inside this legend, the fieldset doesn't disable us.
            let in_legend = self
              .all_ancestors
              .get(i + 1..)
              .into_iter()
              .flatten()
              .any(|n| ptr::eq(*n, *legend));
            if in_legend {
              continue;
            }
          }

          return true;
        }
      }
    }

    if tag.eq_ignore_ascii_case("option") || tag.eq_ignore_ascii_case("optgroup") {
      for ancestor in self.all_ancestors.iter().rev() {
        if let Some(a_tag) = ancestor.tag_name() {
          if a_tag.eq_ignore_ascii_case("select")
            || a_tag.eq_ignore_ascii_case("optgroup")
            || a_tag.eq_ignore_ascii_case("fieldset")
          {
            if ancestor.get_attribute_ref("disabled").is_some() {
              return true;
            }
          }
        }
      }
    }

    false
  }

  fn is_contenteditable(&self) -> bool {
    if !self.is_html_element() {
      return false;
    }
    if let Some(value) = self.node.get_attribute_ref("contenteditable") {
      return value.is_empty() || value.eq_ignore_ascii_case("true");
    }
    false
  }

  fn is_text_editable_input(&self) -> bool {
    if !self.is_html_element() {
      return false;
    }
    let Some(tag) = self.node.tag_name() else {
      return false;
    };
    if !tag.eq_ignore_ascii_case("input") {
      return false;
    }

    match self.node.get_attribute_ref("type") {
      None => true,
      Some(t) => {
        t.eq_ignore_ascii_case("text")
          || t.eq_ignore_ascii_case("search")
          || t.eq_ignore_ascii_case("url")
          || t.eq_ignore_ascii_case("tel")
          || t.eq_ignore_ascii_case("email")
          || t.eq_ignore_ascii_case("password")
          || t.eq_ignore_ascii_case("number")
          || t.eq_ignore_ascii_case("date")
          || t.eq_ignore_ascii_case("datetime-local")
          || t.eq_ignore_ascii_case("month")
          || t.eq_ignore_ascii_case("week")
          || t.eq_ignore_ascii_case("time")
      }
    }
  }

  fn is_option_selected(&self) -> bool {
    if node_hidden_for_select(self.node)
      || self
        .all_ancestors
        .iter()
        .rev()
        .any(|ancestor| node_hidden_for_select(ancestor))
    {
      return false;
    }

    // Find the nearest select ancestor.
    let select = self.all_ancestors.iter().rev().copied().find(|ancestor| {
      ancestor
        .tag_name()
        .map(|t| t.eq_ignore_ascii_case("select"))
        .unwrap_or(false)
    });

    let Some(select_node) = select else {
      // Without a <select>, treat the `selected` attribute as selectedness.
      return self.node.get_attribute_ref("selected").is_some();
    };

    if select_node.get_attribute_ref("multiple").is_some() {
      // Multiple selects have no default selected option; selectedness is driven by the `selected`
      // attribute.
      return self.node.get_attribute_ref("selected").is_some();
    }

    let selected = single_select_selected_option(select_node);
    matches!(selected, Some(opt) if ptr::eq(opt, self.node))
  }

  fn radio_group_name(&self) -> Option<&'a str> {
    self
      .node
      .get_attribute_ref("name")
      .filter(|name| !name.is_empty())
  }

  fn radio_group_root(&self) -> &'a DomNode {
    // Radio group membership is scoped to the nearest ancestor <form> within the current tree
    // root. Shadow roots act as tree-root boundaries, so radios inside shadow trees never group
    // with light-DOM radios, even if the shadow host is itself inside a <form>.
    for ancestor in self.all_ancestors.iter().rev().copied() {
      if ancestor
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
      {
        return ancestor;
      }
      if matches!(
        ancestor.node_type,
        DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. }
      ) {
        return ancestor;
      }
    }

    // Fallback for incomplete ancestor chains (e.g. unit tests constructing partial DOM trees).
    self.all_ancestors.first().copied().unwrap_or(self.node)
  }

  fn last_checked_radio_in_group(root: &'a DomNode, group_name: &str) -> Option<&'a DomNode> {
    let mut last: Option<&'a DomNode> = None;
    let mut stack: Vec<&'a DomNode> = Vec::new();
    stack.push(root);

    while let Some(node) = stack.pop() {
      if node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
      {
        let input_type = node.get_attribute_ref("type").unwrap_or("text");
        if input_type.eq_ignore_ascii_case("radio")
          && node.get_attribute_ref("checked").is_some()
          && node.get_attribute_ref("name") == Some(group_name)
        {
          last = Some(node);
        }
      }

      // Forms and shadow roots are group boundaries:
      // - Controls in different <form> elements are never in the same group.
      // - Shadow roots define independent trees; do not traverse into shadow DOM when scanning a
      //   light DOM radio group (or vice-versa).
      if node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
        && !ptr::eq(node, root)
      {
        continue;
      }

      for child in node.traversal_children().iter().rev() {
        if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
          continue;
        }
        stack.push(child);
      }
    }

    last
  }

  fn is_checked(&self) -> bool {
    let Some(tag) = self.node.tag_name() else {
      return false;
    };

    if tag.eq_ignore_ascii_case("input") {
      let input_type = self.node.get_attribute_ref("type").unwrap_or("text");

      if input_type.eq_ignore_ascii_case("checkbox") {
        return self.node.get_attribute_ref("checked").is_some();
      }

      if input_type.eq_ignore_ascii_case("radio") {
        if self.node.get_attribute_ref("checked").is_none() {
          return false;
        }

        // Unnamed radios are not mutually exclusive, so the presence of the `checked` attribute is
        // sufficient for :checked.
        let Some(name) = self.radio_group_name() else {
          return true;
        };

        let root = self.radio_group_root();
        let last = Self::last_checked_radio_in_group(root, name);
        return last.is_some_and(|node| ptr::eq(node, self.node));
      }

      return false;
    }

    if tag.eq_ignore_ascii_case("option") {
      return self.is_option_selected();
    }

    false
  }

  fn is_read_write(&self) -> bool {
    if self.is_disabled() {
      return false;
    }

    if !self.is_html_element() {
      return false;
    }

    if self.is_text_editable_input() {
      return self.node.get_attribute_ref("readonly").is_none();
    }

    if let Some(tag) = self.node.tag_name() {
      if tag.eq_ignore_ascii_case("textarea") {
        return self.node.get_attribute_ref("readonly").is_none();
      }
      if tag.eq_ignore_ascii_case("select") {
        return true;
      }
    }

    self.is_contenteditable()
  }

  fn supports_required(&self) -> bool {
    if !self.is_html_element() {
      return false;
    }
    let Some(tag) = self.node.tag_name() else {
      return false;
    };

    if tag.eq_ignore_ascii_case("select") || tag.eq_ignore_ascii_case("textarea") {
      return true;
    }

    if tag.eq_ignore_ascii_case("input") {
      let t = self.node.get_attribute_ref("type").unwrap_or("text");
      return !t.eq_ignore_ascii_case("hidden")
        && !t.eq_ignore_ascii_case("button")
        && !t.eq_ignore_ascii_case("reset")
        && !t.eq_ignore_ascii_case("submit")
        && !t.eq_ignore_ascii_case("image")
        && !t.eq_ignore_ascii_case("range")
        && !t.eq_ignore_ascii_case("color");
    }

    false
  }

  fn is_required(&self) -> bool {
    self.supports_required() && self.node.get_attribute_ref("required").is_some()
  }

  fn supports_validation(&self) -> bool {
    if !self.is_html_element() {
      return false;
    }
    let Some(tag) = self.node.tag_name() else {
      return false;
    };
    if tag.eq_ignore_ascii_case("textarea") || tag.eq_ignore_ascii_case("select") {
      return true;
    }

    if tag.eq_ignore_ascii_case("input") {
      let t = self.node.get_attribute_ref("type").unwrap_or("text");
      return !t.eq_ignore_ascii_case("button")
        && !t.eq_ignore_ascii_case("reset")
        && !t.eq_ignore_ascii_case("submit")
        && !t.eq_ignore_ascii_case("image")
        && !t.eq_ignore_ascii_case("hidden");
    }

    false
  }

  fn control_value(&self) -> Option<String> {
    let tag = self.node.tag_name()?;
    if tag.eq_ignore_ascii_case("textarea") {
      return Some(textarea_current_value(self.node));
    }
    if tag.eq_ignore_ascii_case("select") {
      return self.select_value();
    }
    if tag.eq_ignore_ascii_case("input") {
      if let Some(value) = input_range_value(self.node) {
        return Some(format_number(value));
      }
      if let Some(value) = input_color_value_string(self.node) {
        return Some(value);
      }
      if let Some(value) = input_number_value_string(self.node) {
        return Some(value);
      }
      if let Some(value) = input_date_value_string(self.node) {
        return Some(value);
      }
      if let Some(value) = input_time_value_string(self.node) {
        return Some(value);
      }
      if let Some(value) = input_datetime_local_value_string(self.node) {
        return Some(value);
      }
      if let Some(value) = input_month_value_string(self.node) {
        return Some(value);
      }
      if let Some(value) = input_week_value_string(self.node) {
        return Some(value);
      }
      return Some(
        self
          .node
          .get_attribute_ref("value")
          .map(|v| v.to_string())
          .unwrap_or_default(),
      );
    }
    None
  }

  fn select_value(&self) -> Option<String> {
    // Mirror `HTMLSelectElement.value`: the first selected option's value, or the empty string when
    // no options are selected.
    if self.node.get_attribute_ref("multiple").is_some() {
      let value = first_selected_option(self.node)
        .map(option_value_from_node)
        .unwrap_or_default();
      return Some(value);
    }

    let value = single_select_selected_option(self.node)
      .map(option_value_from_node)
      .unwrap_or_default();
    Some(value)
  }

  fn parse_number(value: &str) -> Option<f64> {
    trim_ascii_whitespace_html(value)
      .parse::<f64>()
      .ok()
      .filter(|v| v.is_finite())
  }

  fn numeric_in_range(&self, value: f64) -> Option<bool> {
    let min = self
      .node
      .get_attribute_ref("min")
      .and_then(|m| Self::parse_number(m));
    let max = self
      .node
      .get_attribute_ref("max")
      .and_then(|m| Self::parse_number(m));

    if min.is_none() && max.is_none() {
      return None;
    }

    if let Some(min) = min {
      if value < min {
        return Some(false);
      }
    }
    if let Some(max) = max {
      if value > max {
        return Some(false);
      }
    }
    Some(true)
  }

  fn is_valid_control(&self) -> bool {
    crate::dom::forms_validation::validity_state(self).is_some_and(|state| state.valid)
  }

  fn range_state(&self) -> Option<bool> {
    crate::dom::forms_validation::range_state(self)
  }

  fn is_indeterminate(&self) -> bool {
    let Some(tag) = self.node.tag_name() else {
      return false;
    };
    if tag.eq_ignore_ascii_case("input") {
      let input_type = self.node.get_attribute_ref("type").unwrap_or("text");

      if input_type.eq_ignore_ascii_case("checkbox") {
        return self.node.get_attribute_ref("indeterminate").is_some();
      }
      return false;
    }

    if tag.eq_ignore_ascii_case("progress") {
      // Missing or invalid value makes progress indeterminate.
      let Some(value) = self.node.get_attribute_ref("value") else {
        return true;
      };
      return Self::parse_number(&value).is_none();
    }

    false
  }

  fn nearest_form(&self) -> Option<&DomNode> {
    self.all_ancestors.iter().rev().copied().find(|node| {
      node
        .tag_name()
        .map(|t| t.eq_ignore_ascii_case("form"))
        .unwrap_or(false)
    })
  }

  fn tree_root_info(&self) -> (&'a DomNode, &'a [&'a DomNode]) {
    if self.all_ancestors.is_empty() {
      return (self.node, &[]);
    }

    let mut start = 0usize;
    for (idx, ancestor) in self.all_ancestors.iter().enumerate() {
      if matches!(ancestor.node_type, DomNodeType::ShadowRoot { .. }) {
        start = idx;
      }
    }

    (self.all_ancestors[start], &self.all_ancestors[start..])
  }

  fn collect_forms_by_id<'b>(tree_root: &'b DomNode) -> HashMap<&'b str, &'b DomNode> {
    let mut forms: HashMap<&'b str, &'b DomNode> = HashMap::new();
    let mut stack: Vec<&'b DomNode> = vec![tree_root];

    while let Some(node) = stack.pop() {
      if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) && !ptr::eq(node, tree_root) {
        continue;
      }

      if node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
      {
        if let Some(id) = node.get_attribute_ref("id") {
          forms.entry(id).or_insert(node);
        }
      }

      for child in node.traversal_children().iter().rev() {
        stack.push(child);
      }
    }

    forms
  }

  fn resolve_form_owner_for_node<'b>(
    node: &'b DomNode,
    nearest_form_ancestor: Option<&'b DomNode>,
    forms_by_id: &HashMap<&'b str, &'b DomNode>,
  ) -> Option<&'b DomNode> {
    if let Some(form_attr) = node.get_attribute_ref("form") {
      if let Some(form) = forms_by_id.get(form_attr).copied() {
        return Some(form);
      }
    }
    nearest_form_ancestor
  }

  pub(crate) fn form_owner(&self) -> Option<&DomNode> {
    let Some(tag) = self.node.tag_name() else {
      return None;
    };
    if !(tag.eq_ignore_ascii_case("input")
      || tag.eq_ignore_ascii_case("button")
      || tag.eq_ignore_ascii_case("select")
      || tag.eq_ignore_ascii_case("textarea"))
    {
      return None;
    }

    let (tree_root, ancestors_in_tree) = self.tree_root_info();
    let forms_by_id = Self::collect_forms_by_id(tree_root);

    let nearest_form_ancestor = ancestors_in_tree.iter().rev().copied().find(|node| {
      node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
    });

    Self::resolve_form_owner_for_node(self.node, nearest_form_ancestor, &forms_by_id)
  }

  fn radio_group_is_missing(&self) -> bool {
    forms_validation::radio_group_is_missing(self)
  }

  fn is_default_submit_candidate(node: &DomNode, ancestors: &[&DomNode]) -> bool {
    let Some(tag) = node.tag_name() else {
      return false;
    };

    let is_submit_input = tag.eq_ignore_ascii_case("input")
      && node
        .get_attribute_ref("type")
        .map(|t| t.eq_ignore_ascii_case("submit") || t.eq_ignore_ascii_case("image"))
        .unwrap_or(false);

    let is_button_submit = tag.eq_ignore_ascii_case("button")
      && node
        .get_attribute_ref("type")
        .map(|t| t.eq_ignore_ascii_case("submit"))
        .unwrap_or(true);

    if !(is_submit_input || is_button_submit) {
      return false;
    }

    let element_ref = ElementRef::with_ancestors(node, ancestors);
    !(element_ref.supports_disabled() && element_ref.is_disabled())
  }

  fn is_default_submit(&self) -> bool {
    let Some(form) = self.nearest_form() else {
      return false;
    };

    let mut ancestors = vec![form];
    let target = self.node as *const DomNode;

    fn traverse<'a>(
      node: &'a DomNode,
      ancestors: &mut Vec<&'a DomNode>,
      target: *const DomNode,
    ) -> Option<bool> {
      if ElementRef::is_default_submit_candidate(node, ancestors) {
        return Some(ptr::eq(node, target));
      }

      ancestors.push(node);
      for child in node.children.iter() {
        if let Some(res) = traverse(child, ancestors, target) {
          ancestors.pop();
          return Some(res);
        }
      }
      ancestors.pop();
      None
    }

    traverse(form, &mut ancestors, target).unwrap_or(false)
  }

  /// Direction from dir/xml:dir attributes, inherited; defaults to LTR when none found.
  fn direction(&self) -> TextDirection {
    if let Some(dir) = self.dir_attribute(self.node, self.node) {
      return dir;
    }
    for ancestor in self.all_ancestors.iter().rev() {
      if let Some(dir) = self.dir_attribute(ancestor, ancestor) {
        return dir;
      }
    }
    TextDirection::Ltr
  }

  fn dir_attribute(&self, node: &DomNode, resolve_root: &DomNode) -> Option<TextDirection> {
    node
      .get_attribute_ref("dir")
      .or_else(|| node.get_attribute_ref("xml:dir"))
      .and_then(|d| {
        if d.eq_ignore_ascii_case("ltr") {
          return Some(TextDirection::Ltr);
        }
        if d.eq_ignore_ascii_case("rtl") {
          return Some(TextDirection::Rtl);
        }
        if d.eq_ignore_ascii_case("auto") {
          return resolve_first_strong_direction(resolve_root);
        }
        None
      })
  }

  fn is_placeholder_shown(&self) -> bool {
    let Some(tag) = self.node.tag_name() else {
      return false;
    };

    if tag.eq_ignore_ascii_case("input") {
      if self.node.get_attribute_ref("placeholder").is_none() {
        return false;
      }

      let input_type = self.node.get_attribute_ref("type");

      if !supports_placeholder(input_type) {
        return false;
      }

      return self
        .node
        .get_attribute_ref("value")
        .unwrap_or("")
        .is_empty();
    }

    if tag.eq_ignore_ascii_case("textarea") {
      if self.node.get_attribute_ref("placeholder").is_none() {
        return false;
      }

      let value = textarea_current_value(self.node);
      return value.is_empty();
    }

    false
  }

  /// Expose the disabled state for accessibility mapping.
  pub(crate) fn accessibility_disabled(&self) -> bool {
    self.is_disabled()
  }

  /// Expose the checked state for accessibility mapping.
  pub(crate) fn accessibility_checked(&self) -> bool {
    self.is_checked()
  }

  /// Expose whether an option-like element is selected.
  pub(crate) fn accessibility_selected(&self) -> bool {
    self.is_option_selected()
  }

  /// Expose whether the control is indeterminate (checkbox/progress).
  pub(crate) fn accessibility_indeterminate(&self) -> bool {
    self.is_indeterminate()
  }

  /// Expose whether the control is required.
  pub(crate) fn accessibility_required(&self) -> bool {
    self.is_required()
  }

  /// Expose whether the control is read-only for accessibility mapping.
  pub(crate) fn accessibility_readonly(&self) -> bool {
    if let Some(tag) = self.node.tag_name() {
      if tag.eq_ignore_ascii_case("textarea") {
        return self.node.get_attribute_ref("readonly").is_some();
      }
      if tag.eq_ignore_ascii_case("input") && self.is_text_editable_input() {
        return self.node.get_attribute_ref("readonly").is_some();
      }
    }

    false
  }

  /// Expose whether the control supports constraint validation.
  pub(crate) fn accessibility_supports_validation(&self) -> bool {
    self.supports_validation()
  }

  /// Expose whether the control is valid according to HTML form rules.
  pub(crate) fn accessibility_is_valid(&self) -> bool {
    self.is_valid_control()
  }

  /// Expose control value for form controls (input/select/textarea).
  pub(crate) fn accessibility_value(&self) -> Option<String> {
    self.control_value()
  }

  fn is_target(&self) -> bool {
    current_target_fragment()
      .as_deref()
      .map(|target| Self::node_matches_target(self.node, target))
      .unwrap_or(false)
  }

  fn subtree_contains_target(&self, slot_map: Option<&SlotAssignmentMap<'_>>) -> bool {
    let Some(target) = current_target_fragment() else {
      return false;
    };
    Self::subtree_has_target(self.node, target.as_str(), slot_map)
  }

  fn subtree_has_target(
    node: &DomNode,
    target: &str,
    slot_map: Option<&SlotAssignmentMap<'_>>,
  ) -> bool {
    let mut stack: Vec<&DomNode> = vec![node];
    let mut visited = slot_map.is_some().then(HashSet::new);

    while let Some(current) = stack.pop() {
      if let (Some(map), Some(ref mut seen)) = (slot_map, visited.as_mut()) {
        if let Some(id) = map.node_id(current) {
          if !seen.insert(id) {
            continue;
          }
        }
      }

      if Self::node_matches_target(current, target) {
        return true;
      }

      let assigned_children_pushed =
        Self::push_assigned_slot_nodes(current, slot_map, visited.as_ref(), &mut stack);

      if !assigned_children_pushed {
        for child in current.children.iter().rev() {
          stack.push(child);
        }
      }
    }

    false
  }

  fn node_matches_target(node: &DomNode, target: &str) -> bool {
    if let Some(id) = node.get_attribute_ref("id") {
      if id == target {
        return true;
      }
    }

    if let Some(tag) = node.tag_name() {
      if tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area") {
        if let Some(name) = node.get_attribute_ref("name") {
          if name == target {
            return true;
          }
        }
      }
    }

    false
  }
}

/// Compute the raw textarea value, normalizing newline conventions and removing the single leading
/// newline that HTML ignores when contents start with a line break (common with formatted markup).
pub(crate) fn textarea_value(node: &DomNode) -> String {
  let mut value = String::new();
  for child in node.children.iter() {
    if let DomNodeType::Text { content } = &child.node_type {
      value.push_str(content);
    }
  }

  normalize_textarea_value(value)
}

pub(crate) fn normalize_textarea_newlines(value: String) -> String {
  let mut value = value;
  if value.contains('\r') {
    let mut normalized = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
      if ch == '\r' {
        if matches!(chars.peek(), Some('\n')) {
          chars.next();
        }
        normalized.push('\n');
      } else {
        normalized.push(ch);
      }
    }
    value = normalized;
  }

  value
}

pub(crate) fn normalize_textarea_value(value: String) -> String {
  let mut value = normalize_textarea_newlines(value);
  if value.starts_with('\n') {
    value.remove(0);
  }

  value
}

pub(crate) fn textarea_current_value(node: &DomNode) -> String {
  if let Some(value) = node.get_attribute_ref("data-fastr-value") {
    return normalize_textarea_newlines(value.to_string());
  }

  textarea_value(node)
}

pub(crate) fn textarea_current_value_from_text_content(node: &DomNode, text_content: String) -> String {
  if let Some(value) = node.get_attribute_ref("data-fastr-value") {
    return normalize_textarea_newlines(value.to_string());
  }

  normalize_textarea_value(text_content)
}

fn matches_an_plus_b(a: i32, b: i32, position: i32) -> bool {
  if a == 0 {
    position == b
  } else {
    let diff = position - b;
    diff % a == 0 && diff / a >= 0
  }
}

fn language_tag_is_normalized(tag: &str) -> bool {
  let bytes = tag.as_bytes();
  if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
    return false;
  }

  let mut prev_was_sep = false;
  for &b in bytes {
    if b == b'_' {
      return false;
    }
    if (b'A'..=b'Z').contains(&b) {
      return false;
    }
    if b == b'-' {
      if prev_was_sep {
        return false;
      }
      prev_was_sep = true;
    } else {
      prev_was_sep = false;
    }
  }

  true
}

fn normalize_language_tag_for_selector_matching<'a>(tag: &'a str) -> Cow<'a, str> {
  let trimmed = trim_ascii_whitespace_html(tag);
  if trimmed.is_empty() {
    return Cow::Borrowed("");
  }

  if language_tag_is_normalized(trimmed) {
    return Cow::Borrowed(trimmed);
  }

  let mut out = String::with_capacity(trimmed.len());
  let mut last_was_sep = true;
  for ch in trimmed.chars() {
    let ch = if ch == '_' { '-' } else { ch };
    if ch == '-' {
      if last_was_sep {
        continue;
      }
      out.push('-');
      last_was_sep = true;
    } else {
      out.push(ch.to_ascii_lowercase());
      last_was_sep = false;
    }
  }
  if last_was_sep {
    out.pop();
  }
  Cow::Owned(out)
}

fn lang_matches(range: &str, lang: &str) -> bool {
  if range == "*" {
    return !lang.is_empty();
  }
  if range == lang {
    return true;
  }
  // Prefix match with boundary.
  let Some(prefix) = lang.as_bytes().get(..range.len()) else {
    return false;
  };
  prefix == range.as_bytes() && lang.as_bytes().get(range.len()) == Some(&b'-')
}

pub(crate) fn supports_placeholder(input_type: Option<&str>) -> bool {
  let Some(t) = input_type else {
    return true;
  };

  // Per HTML spec, placeholder is supported for text-like controls; unknown types default to text.
  if t.eq_ignore_ascii_case("text")
    || t.eq_ignore_ascii_case("search")
    || t.eq_ignore_ascii_case("url")
    || t.eq_ignore_ascii_case("tel")
    || t.eq_ignore_ascii_case("email")
    || t.eq_ignore_ascii_case("password")
    || t.eq_ignore_ascii_case("number")
  {
    return true;
  }

  if t.eq_ignore_ascii_case("hidden")
    || t.eq_ignore_ascii_case("submit")
    || t.eq_ignore_ascii_case("reset")
    || t.eq_ignore_ascii_case("button")
    || t.eq_ignore_ascii_case("image")
    || t.eq_ignore_ascii_case("file")
    || t.eq_ignore_ascii_case("checkbox")
    || t.eq_ignore_ascii_case("radio")
    || t.eq_ignore_ascii_case("range")
    || t.eq_ignore_ascii_case("color")
    || t.eq_ignore_ascii_case("date")
    || t.eq_ignore_ascii_case("datetime-local")
    || t.eq_ignore_ascii_case("month")
    || t.eq_ignore_ascii_case("week")
    || t.eq_ignore_ascii_case("time")
  {
    return false;
  }

  true
}

fn inline_style_display_is_none(node: &DomNode) -> Option<bool> {
  let style_attr = node.get_attribute_ref("style")?;
  let mut display_is_none = None;
  for decl in style_attr.split(';') {
    let Some((name, value)) = decl.split_once(':') else {
      continue;
    };
    if !trim_ascii_whitespace_html(name).eq_ignore_ascii_case("display") {
      continue;
    }
    let token = trim_ascii_whitespace_html(value)
      .split(|c: char| is_ascii_whitespace_html(c) || c == '!' || c == ';')
      .next()
      .unwrap_or("");
    if token.is_empty() {
      continue;
    }
    // Inline style declarations follow standard CSS rules: later declarations override earlier ones.
    display_is_none = Some(token.eq_ignore_ascii_case("none"));
  }
  display_is_none
}

fn node_hidden_for_select(node: &DomNode) -> bool {
  // The CSS cascade is not available at selector-matching time, but `<option hidden>`
  // / `<optgroup hidden>` should still not participate in selection heuristics.
  //
  // Honor inline `style="display: ..."` when present so author overrides of `[hidden]`
  // stay consistent with the computed `display` used by the rendering pipeline.
  if let Some(is_none) = inline_style_display_is_none(node) {
    return is_none;
  }
  let DomNodeType::Element { attributes, .. } = &node.node_type else {
    return false;
  };
  node_is_hidden(attributes)
}

pub(crate) fn strip_and_collapse_ascii_whitespace(value: &str) -> String {
  fn is_ascii_ws(c: char) -> bool {
    matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}')
  }

  let mut out = String::with_capacity(value.len());
  let mut pending_space = false;

  for c in value.chars() {
    if is_ascii_ws(c) {
      if !out.is_empty() {
        pending_space = true;
      }
      continue;
    }
    if pending_space {
      out.push(' ');
      pending_space = false;
    }
    out.push(c);
  }

  out
}

fn collect_descendant_text_content(node: &DomNode) -> String {
  let mut text = String::new();
  let mut stack: Vec<&DomNode> = Vec::new();
  stack.push(node);

  while let Some(node) = stack.pop() {
    match &node.node_type {
      DomNodeType::Text { content } => text.push_str(content),
      DomNodeType::Element {
        tag_name,
        namespace,
        ..
      } => {
        if tag_name.eq_ignore_ascii_case("script")
          && (namespace.is_empty() || namespace == HTML_NAMESPACE || namespace == SVG_NAMESPACE)
        {
          continue;
        }
      }
      _ => {}
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  text
}

fn option_value_from_node(node: &DomNode) -> String {
  if let DomNodeType::Element { attributes, .. } = &node.node_type {
    if let Some((_, v)) = attributes
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case("value"))
    {
      return v.clone();
    }
  }

  strip_and_collapse_ascii_whitespace(&collect_descendant_text_content(node))
}

fn first_selected_option<'a>(select: &'a DomNode) -> Option<&'a DomNode> {
  let mut stack: Vec<&'a DomNode> = Vec::new();
  stack.push(select);

  while let Some(node) = stack.pop() {
    if node_hidden_for_select(node) {
      continue;
    }

    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
      && node.get_attribute_ref("selected").is_some()
    {
      return Some(node);
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  None
}

fn single_select_selected_option<'a>(select: &'a DomNode) -> Option<&'a DomNode> {
  single_select_selected_option_and_disabled(select).map(|(node, _)| node)
}

fn single_select_selected_option_and_disabled<'a>(select: &'a DomNode) -> Option<(&'a DomNode, bool)> {
  let mut first_option: Option<(&'a DomNode, bool)> = None;
  let mut first_enabled_option: Option<(&'a DomNode, bool)> = None;
  let mut last_selected_option: Option<(&'a DomNode, bool)> = None;

  let mut stack: Vec<(&'a DomNode, bool)> = Vec::new();
  stack.push((select, false));

  while let Some((node, optgroup_disabled)) = stack.pop() {
    if node_hidden_for_select(node) {
      continue;
    }
    let tag = node.tag_name().unwrap_or("");
    let is_option = tag.eq_ignore_ascii_case("option");
    let is_optgroup = tag.eq_ignore_ascii_case("optgroup");

    let disabled_attr = node.get_attribute_ref("disabled").is_some();
    let next_optgroup_disabled = optgroup_disabled || (is_optgroup && disabled_attr);

    if is_option {
      let disabled = disabled_attr || optgroup_disabled;
      if first_option.is_none() {
        first_option = Some((node, disabled));
      }

      if first_enabled_option.is_none() && !disabled {
        first_enabled_option = Some((node, false));
      }

      if node.get_attribute_ref("selected").is_some() {
        last_selected_option = Some((node, disabled));
      }
    }

    for child in node.children.iter().rev() {
      stack.push((child, next_optgroup_disabled));
    }
  }

  last_selected_option
    .or(first_enabled_option)
    .or(first_option)
}

fn select_display_size(select: &DomNode) -> u32 {
  let size = select
    .get_attribute_ref("size")
    .and_then(|value| trim_ascii_whitespace_html(value).parse::<u32>().ok())
    .filter(|value| *value > 0);

  size.unwrap_or_else(|| {
    if select.get_attribute_ref("multiple").is_some() {
      4
    } else {
      1
    }
  })
}

fn select_placeholder_label_option<'a>(select: &'a DomNode) -> Option<&'a DomNode> {
  if select.get_attribute_ref("required").is_none() {
    return None;
  }
  if select.get_attribute_ref("multiple").is_some() {
    return None;
  }
  if select_display_size(select) != 1 {
    return None;
  }

  let mut stack: Vec<(&'a DomNode, Option<&'a DomNode>)> = Vec::new();
  stack.push((select, None));

  while let Some((node, parent)) = stack.pop() {
    if node_hidden_for_select(node) {
      continue;
    }

    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      let value = option_value_from_node(node);
      if value.is_empty() && matches!(parent, Some(parent) if ptr::eq(parent, select)) {
        return Some(node);
      }
      return None;
    }

    for child in node.children.iter().rev() {
      stack.push((child, Some(node)));
    }
  }

  None
}

fn select_has_non_disabled_selected_option(select: &DomNode) -> bool {
  if select.get_attribute_ref("multiple").is_none() {
    return single_select_selected_option_and_disabled(select).is_some_and(|(_, disabled)| !disabled);
  }

  let mut stack: Vec<(&DomNode, bool)> = Vec::new();
  stack.push((select, false));

  while let Some((node, optgroup_disabled)) = stack.pop() {
    if node_hidden_for_select(node) {
      continue;
    }
    let tag = node.tag_name().unwrap_or("");
    let is_option = tag.eq_ignore_ascii_case("option");
    let is_optgroup = tag.eq_ignore_ascii_case("optgroup");

    let disabled_attr = node.get_attribute_ref("disabled").is_some();
    let next_optgroup_disabled = optgroup_disabled || (is_optgroup && disabled_attr);

    if is_option && node.get_attribute_ref("selected").is_some() {
      let option_disabled = disabled_attr || optgroup_disabled;
      if !option_disabled {
        return true;
      }
    }

    for child in node.children.iter().rev() {
      stack.push((child, next_optgroup_disabled));
    }
  }

  false
}

pub(crate) fn parse_select_size_attribute(node: &DomNode) -> Option<u32> {
  let raw = node.get_attribute_ref("size")?;
  let trimmed = trim_ascii_whitespace_html(raw);
  if trimmed.is_empty() {
    return None;
  }
  let parsed = trimmed.parse::<i64>().ok()?;
  if parsed <= 0 {
    return None;
  }
  u32::try_from(parsed).ok()
}

pub(crate) fn select_is_listbox(node: &DomNode) -> bool {
  if node.get_attribute_ref("multiple").is_some() {
    return true;
  }
  matches!(parse_select_size_attribute(node), Some(size) if size > 1)
}

pub(crate) fn select_effective_size(node: &DomNode) -> u32 {
  let multiple = node.get_attribute_ref("multiple").is_some();
  let size_attr = parse_select_size_attribute(node);
  if multiple {
    size_attr.unwrap_or(4)
  } else {
    size_attr.filter(|&size| size > 1).unwrap_or(1)
  }
}

impl<'a> Element for ElementRef<'a> {
  type Impl = FastRenderSelectorImpl;

  fn opaque(&self) -> OpaqueElement {
    OpaqueElement::new(self.node)
  }

  fn parent_element(&self) -> Option<Self> {
    let parent = self.parent?;
    if !parent.is_element() {
      return None;
    }

    Some(if self.all_ancestors.len() > 1 {
      // If we have multiple ancestors, the parent's ancestors are all but the last
      ElementRef::with_ancestors(parent, &self.all_ancestors[..self.all_ancestors.len() - 1])
        .with_slot_map(self.slot_map)
        .with_attr_cache(self.attr_cache)
    } else {
      // Parent is the root
      ElementRef::new(parent)
        .with_slot_map(self.slot_map)
        .with_attr_cache(self.attr_cache)
    })
  }

  fn parent_node_is_shadow_root(&self) -> bool {
    matches!(
      self.parent.map(|p| &p.node_type),
      Some(DomNodeType::ShadowRoot { .. })
    )
  }

  fn containing_shadow_host(&self) -> Option<Self> {
    for (idx, ancestor) in self.all_ancestors.iter().enumerate().rev() {
      if matches!(ancestor.node_type, DomNodeType::ShadowRoot { .. }) {
        if idx == 0 {
          return None;
        }
        let host = self.all_ancestors[idx - 1];
        return Some(
          ElementRef::with_ancestors(host, &self.all_ancestors[..idx - 1])
            .with_slot_map(self.slot_map)
            .with_attr_cache(self.attr_cache),
        );
      }
    }
    None
  }

  fn is_pseudo_element(&self) -> bool {
    false
  }

  fn prev_sibling_element(&self) -> Option<Self> {
    let parent = self.parent?;
    let mut prev: Option<&DomNode> = None;
    for child in parent.traversal_children().iter() {
      if !child.is_element() {
        continue;
      }
      if ptr::eq(child, self.node) {
        return prev.map(|node| ElementRef {
          node,
          node_id: 0,
          parent: self.parent,
          all_ancestors: self.all_ancestors,
          slot_map: self.slot_map,
          attr_cache: self.attr_cache,
        });
      }
      prev = Some(child);
    }
    None
  }

  fn next_sibling_element(&self) -> Option<Self> {
    let parent = self.parent?;
    let mut seen_self = false;
    for child in parent.traversal_children().iter() {
      if !child.is_element() {
        continue;
      }
      if seen_self {
        return Some(ElementRef {
          node: child,
          node_id: 0,
          parent: self.parent,
          all_ancestors: self.all_ancestors,
          slot_map: self.slot_map,
          attr_cache: self.attr_cache,
        });
      }
      if ptr::eq(child, self.node) {
        seen_self = true;
      }
    }
    None
  }

  fn is_html_element_in_html_document(&self) -> bool {
    match &self.node.node_type {
      DomNodeType::Element { namespace, .. } | DomNodeType::Slot { namespace, .. } => {
        namespace.is_empty() || namespace == HTML_NAMESPACE
      }
      _ => false,
    }
  }

  fn has_local_name(&self, local_name: &str) -> bool {
    self.node.tag_name().is_some_and(|tag| {
      if self.is_html_element() {
        tag.eq_ignore_ascii_case(local_name)
      } else {
        tag == local_name
      }
    })
  }

  fn has_namespace(&self, ns: &str) -> bool {
    match &self.node.node_type {
      DomNodeType::Element { namespace, .. } | DomNodeType::Slot { namespace, .. } => {
        // The selectors crate uses an empty namespace URL to represent the explicit "no namespace"
        // selector form (`|E`). FastRender stores HTML elements with an empty string namespace to
        // save memory, so treat an empty selector namespace as "no namespace" (which never matches
        // HTML).
        if ns.is_empty() {
          return false;
        }
        if namespace == ns {
          return true;
        }
        namespace.is_empty() && ns == HTML_NAMESPACE
      }
      _ => false,
    }
  }

  fn is_same_type(&self, other: &Self) -> bool {
    match (&self.node.node_type, &other.node.node_type) {
      (
        DomNodeType::Element {
          tag_name: a,
          namespace: a_ns,
          ..
        },
        DomNodeType::Element {
          tag_name: b,
          namespace: b_ns,
          ..
        },
      ) if a_ns == b_ns => {
        if a_ns == HTML_NAMESPACE || a_ns.is_empty() {
          a.eq_ignore_ascii_case(b)
        } else {
          a == b
        }
      }
      (
        DomNodeType::Slot {
          namespace: a_ns, ..
        },
        DomNodeType::Slot {
          namespace: b_ns, ..
        },
      ) if a_ns == b_ns => true,
      _ => false,
    }
  }

  fn attr_matches(
    &self,
    ns: &selectors::attr::NamespaceConstraint<&CssString>,
    local_name: &CssString,
    operation: &AttrSelectorOperation<&CssString>,
  ) -> bool {
    // Namespace check: we only support HTML namespace/none.
    match ns {
      selectors::attr::NamespaceConstraint::Any => {}
      selectors::attr::NamespaceConstraint::Specific(url) => {
        let url: &str = (*url).borrow();
        if !(url.is_empty() || url == HTML_NAMESPACE) {
          return false;
        }
      }
    }

    let attr_value = if let Some(cache) = self.attr_cache {
      cache.attr_value(self.node, local_name.as_str())
    } else {
      let is_html = self.is_html_element();
      self
        .node
        .attributes_iter()
        .find(|(name, _)| element_attr_cache_name_matches(name, local_name.as_str(), is_html))
        .map(|(_, value)| value)
    };
    let attr_value = match attr_value {
      Some(v) => v,
      None => return false,
    };

    match operation {
      AttrSelectorOperation::Exists => true,
      AttrSelectorOperation::WithValue {
        operator,
        case_sensitivity,
        value,
      } => {
        let value_str: &str = std::borrow::Borrow::borrow(&**value);
        operator.eval_str(attr_value, value_str, *case_sensitivity)
      }
    }
  }

  fn match_non_ts_pseudo_class(
    &self,
    pseudo: &PseudoClass,
    _context: &mut selectors::matching::MatchingContext<Self::Impl>,
  ) -> bool {
    match pseudo {
      PseudoClass::Has(relative) => {
        _context.with_featureless(false, |context| {
          matches_has_relative(self, relative, context)
        })
      }
      PseudoClass::Host(selectors) => {
        if !_context
          .extra_data
          .shadow_host
          .is_some_and(|host| host == self.opaque())
        {
          return false;
        }
        let Some(selectors) = selectors else {
          return true;
        };
        _context.with_featureless(false, |context| {
          selectors
            .slice()
            .iter()
            .any(|selector| matches_selector(selector, 0, None, self, context))
        })
      }
      PseudoClass::HostContext(selectors) => {
        if !_context
          .extra_data
          .shadow_host
          .is_some_and(|host| host == self.opaque())
        {
          return false;
        }
        _context.with_featureless(false, |context| {
          if selectors
            .slice()
            .iter()
            .any(|selector| matches_selector(selector, 0, None, self, context))
          {
            return true;
          }

          for (idx, ancestor) in self.all_ancestors.iter().enumerate() {
            if !ancestor.is_element() {
              continue;
            }
            let ancestor_ref = ElementRef::with_ancestors(*ancestor, &self.all_ancestors[..idx])
              .with_slot_map(self.slot_map)
              .with_attr_cache(self.attr_cache);
            if selectors
              .slice()
              .iter()
              .any(|selector| matches_selector(selector, 0, None, &ancestor_ref, context))
            {
              return true;
            }
          }
          false
        })
      }
      PseudoClass::Root => {
        matches!(self.node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
          && self
            .node
            .tag_name()
            .map(|t| t.eq_ignore_ascii_case("html"))
            .unwrap_or(false)
      }
      PseudoClass::Defined => {
        if _context.extra_data.treat_custom_elements_as_defined {
          true
        } else {
          // Spec-correct behavior: elements with a valid custom-element name (minimum: contains a
          // hyphen) are undefined unless upgraded by the custom elements registry, which FastRender
          // does not run.
          //
          // Non-custom element names (no hyphen) remain `:defined`.
          !(self.is_html_element() && self.node.tag_name().is_some_and(|tag| tag.contains('-')))
        }
      }
      PseudoClass::FirstChild => self.element_index(_context) == Some(0),
      PseudoClass::LastChild => self
        .element_index_and_len(_context)
        .map(|(idx, len)| idx == len.saturating_sub(1))
        .unwrap_or(false),
      PseudoClass::OnlyChild => self
        .element_index_and_len(_context)
        .map(|(_, len)| len == 1)
        .unwrap_or(false),
      PseudoClass::NthChild(a, b, of) => match of {
        Some(selectors) => {
          let cached = {
            let cache = _context.nth_index_cache(false, false, selectors.slice());
            cache.lookup(self.opaque())
          };
          let index = match cached {
            Some(index) => index,
            None => match self.populate_nth_index_cache_for_selectors(selectors, false, _context) {
              Some(index) => index,
              None => return false,
            },
          };
          index > 0 && matches_an_plus_b(*a, *b, index)
        }
        None => self
          .element_index(_context)
          .map(|index| matches_an_plus_b(*a, *b, (index + 1) as i32))
          .unwrap_or(false),
      },
      PseudoClass::NthLastChild(a, b, of) => match of {
        Some(selectors) => {
          let cached = {
            let cache = _context.nth_index_cache(false, true, selectors.slice());
            cache.lookup(self.opaque())
          };
          let index = match cached {
            Some(index) => index,
            None => match self.populate_nth_index_cache_for_selectors(selectors, true, _context) {
              Some(index) => index,
              None => return false,
            },
          };
          index > 0 && matches_an_plus_b(*a, *b, index)
        }
        None => self
          .element_index_and_len(_context)
          .map(|(index, len)| {
            let n = (len - index) as i32;
            matches_an_plus_b(*a, *b, n)
          })
          .unwrap_or(false),
      },
      PseudoClass::FirstOfType => self
        .position_in_type(_context)
        .map(|(index, _)| index == 0)
        .unwrap_or(false),
      PseudoClass::LastOfType => self
        .position_in_type(_context)
        .map(|(index, len)| index == len.saturating_sub(1))
        .unwrap_or(false),
      PseudoClass::OnlyOfType => self
        .position_in_type(_context)
        .map(|(_, len)| len == 1)
        .unwrap_or(false),
      PseudoClass::NthOfType(a, b) => self
        .position_in_type(_context)
        .map(|(index, _)| matches_an_plus_b(*a, *b, (index + 1) as i32))
        .unwrap_or(false),
      PseudoClass::NthLastOfType(a, b) => self
        .position_in_type(_context)
        .map(|(index, len)| {
          let n = (len - index) as i32;
          matches_an_plus_b(*a, *b, n)
        })
        .unwrap_or(false),
      PseudoClass::Lang(langs) => {
        if let Some(lang) = self.language() {
          langs.iter().any(|range| lang_matches(range, lang.as_ref()))
        } else {
          false
        }
      }
      PseudoClass::Dir(dir) => self.direction() == *dir,
      PseudoClass::AnyLink => self.is_link(),
      PseudoClass::Target => self.is_target(),
      PseudoClass::TargetWithin => self.subtree_contains_target(_context.extra_data.slot_map),
      PseudoClass::Scope => match _context.relative_selector_anchor() {
        Some(anchor) => anchor == self.opaque(),
        None => !self
          .all_ancestors
          .iter()
          .any(|ancestor| ancestor.is_element()),
      },
      PseudoClass::Empty => self.is_empty(),
      PseudoClass::Disabled => self.supports_disabled() && self.is_disabled(),
      PseudoClass::Enabled => self.supports_disabled() && !self.is_disabled(),
      // `:required`/`:optional` are about the element's requiredness flag, not whether it is
      // currently enabled. Disabled controls still participate in these pseudo-classes in modern
      // browsers.
      PseudoClass::Required => self.is_required(),
      PseudoClass::Optional => self.supports_required() && !self.is_required(),
      PseudoClass::Valid => {
        if self.is_html_element() {
          if let Some(tag) = self.node.tag_name() {
            if tag.eq_ignore_ascii_case("form") {
              return _context
                .extra_data
                .form_validity_index
                .is_some_and(|index| !index.form_is_invalid(self.node));
            }
            if tag.eq_ignore_ascii_case("fieldset") {
              return _context
                .extra_data
                .form_validity_index
                .is_some_and(|index| !index.fieldset_is_invalid(self.node));
            }
          }
        }

        (self.supports_validation() && self.is_disabled())
          || (self.supports_validation() && self.is_valid_control())
      }
      PseudoClass::Invalid => {
        if self.is_html_element() {
          if let Some(tag) = self.node.tag_name() {
            if tag.eq_ignore_ascii_case("form") {
              return _context
                .extra_data
                .form_validity_index
                .is_some_and(|index| index.form_is_invalid(self.node));
            }
            if tag.eq_ignore_ascii_case("fieldset") {
              return _context
                .extra_data
                .form_validity_index
                .is_some_and(|index| index.fieldset_is_invalid(self.node));
            }
          }
        }

        self.supports_validation() && !self.is_disabled() && !self.is_valid_control()
      }
      PseudoClass::UserValid => {
        self.user_validity_flag()
          && self.supports_validation()
          && !self.is_disabled()
          && self.is_valid_control()
      }
      PseudoClass::UserInvalid => {
        self.user_validity_flag()
          && self.supports_validation()
          && !self.is_disabled()
          && !self.is_valid_control()
      }
      PseudoClass::InRange => !self.is_disabled() && self.range_state() == Some(true),
      PseudoClass::OutOfRange => !self.is_disabled() && self.range_state() == Some(false),
      PseudoClass::Indeterminate => self.is_indeterminate(),
      PseudoClass::Default => {
        if let Some(tag) = self.node.tag_name() {
          if tag.eq_ignore_ascii_case("option") {
            return self.is_checked();
          }
          if tag.eq_ignore_ascii_case("input") {
            let t = self.node.get_attribute_ref("type").unwrap_or("text");
            if t.eq_ignore_ascii_case("checkbox") || t.eq_ignore_ascii_case("radio") {
              return self.node.get_attribute_ref("checked").is_some();
            }
          }
          if tag.eq_ignore_ascii_case("input") || tag.eq_ignore_ascii_case("button") {
            return self.is_default_submit();
          }
        }
        false
      }
      PseudoClass::ReadOnly => !self.is_read_write(),
      PseudoClass::ReadWrite => self.is_read_write(),
      PseudoClass::PlaceholderShown
      | PseudoClass::WebkitInputPlaceholder
      | PseudoClass::MsInputPlaceholder
      | PseudoClass::MozPlaceholder => self.is_placeholder_shown(),
      PseudoClass::MozUiInvalid | PseudoClass::MozFocusring => false,
      PseudoClass::Autofill => false,
      // Interactive pseudo-classes (not supported in static rendering)
      PseudoClass::Hover => self.hover_flag(),
      PseudoClass::Focus => self.focus_flag(),
      PseudoClass::FocusWithin => self.subtree_contains_focus(_context.extra_data.slot_map),
      PseudoClass::FocusVisible => self.focus_visible_flag(),
      PseudoClass::Fullscreen => false,
      PseudoClass::Open => {
        if self
          .node
          .tag_name()
          .is_some_and(|t| t.eq_ignore_ascii_case("dialog"))
        {
          dialog_state(self.node).is_some()
        } else if self.node.get_attribute_ref("popover").is_some() {
          popover_open_assuming_popover(self.node)
        } else {
          self.node.get_attribute_ref("open").is_some()
        }
      }
      PseudoClass::Modal => dialog_state(self.node).is_some_and(|(_, modal)| modal),
      PseudoClass::PopoverOpen => popover_open(self.node),
      PseudoClass::Active => self.active_flag(),
      PseudoClass::Checked => self.is_checked(),
      PseudoClass::Link => self.is_link() && !self.visited_flag(),
      PseudoClass::Visited => self.is_link() && self.visited_flag(),
      PseudoClass::Vendor(_) => false,
    }
  }

  fn match_pseudo_element(
    &self,
    pseudo: &PseudoElement,
    _context: &mut selectors::matching::MatchingContext<Self::Impl>,
  ) -> bool {
    match pseudo {
      PseudoElement::Placeholder => {
        self.is_html_element() && self.is_placeholder_shown()
      }
      PseudoElement::FileSelectorButton => {
        if !self.is_html_element() {
          return false;
        }

        match self.node.tag_name() {
          Some(tag) if tag.eq_ignore_ascii_case("input") => self
            .node
            .get_attribute_ref("type")
            .unwrap_or("text")
            .eq_ignore_ascii_case("file"),
          _ => false,
        }
      }
      PseudoElement::SliderThumb | PseudoElement::SliderTrack => {
        if !self.is_html_element() {
          return false;
        }

        match self.node.tag_name() {
          Some(tag) if tag.eq_ignore_ascii_case("input") => self
            .node
            .get_attribute_ref("type")
            .unwrap_or("text")
            .eq_ignore_ascii_case("range"),
          _ => false,
        }
      }
      PseudoElement::ProgressBar | PseudoElement::ProgressValue => {
        if !self.is_html_element() {
          return false;
        }
        matches!(self.node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("progress"))
      }
      PseudoElement::MeterBar
      | PseudoElement::MeterOptimumValue
      | PseudoElement::MeterSuboptimumValue
      | PseudoElement::MeterEvenLessGoodValue => {
        if !self.is_html_element() {
          return false;
        }
        matches!(self.node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("meter"))
      }
      // These pseudo-elements are supported for all elements; filtering
      // based on box generation happens later in the pipeline.
      PseudoElement::Before
      | PseudoElement::After
      | PseudoElement::FirstLine
      | PseudoElement::FirstLetter
      | PseudoElement::Marker
      | PseudoElement::Backdrop
      | PseudoElement::FootnoteCall
      | PseudoElement::FootnoteMarker => true,
      PseudoElement::MozFocusInner | PseudoElement::MozFocusOuter => false,
      PseudoElement::Selection => false,
      PseudoElement::Vendor(_) => false,
    }
  }

  fn is_link(&self) -> bool {
    let Some(tag) = self.node.tag_name() else {
      return false;
    };
    let has_href = self.node.get_attribute_ref("href").is_some();
    has_href
      && (tag.eq_ignore_ascii_case("a")
        || tag.eq_ignore_ascii_case("area")
        || tag.eq_ignore_ascii_case("link"))
  }

  fn is_html_slot_element(&self) -> bool {
    self
      .node
      .tag_name()
      .is_some_and(|t| t.eq_ignore_ascii_case("slot"))
  }

  fn assigned_slot(&self) -> Option<Self> {
    let slot_map = self.slot_map?;
    let slot = slot_map.assigned_slot(self.node)?;
    let parent = slot.ancestors.last().copied();
    Some(ElementRef {
      node: slot.slot,
      node_id: 0,
      parent,
      all_ancestors: slot.ancestors,
      slot_map: Some(slot_map),
      attr_cache: self.attr_cache,
    })
  }

  fn has_id(&self, id: &CssString, case_sensitivity: CaseSensitivity) -> bool {
    if let Some(cache) = self.attr_cache {
      return cache.has_id(self.node, id.as_str(), case_sensitivity);
    }

    let is_html = self.is_html_element();
    let actual = self
      .node
      .attributes_iter()
      .find(|(name, _)| element_attr_cache_name_matches(name, "id", is_html))
      .map(|(_, value)| value);
    let Some(actual) = actual else {
      return false;
    };
    match case_sensitivity {
      CaseSensitivity::CaseSensitive => actual == id.as_str(),
      CaseSensitivity::AsciiCaseInsensitive => actual.eq_ignore_ascii_case(id.as_str()),
    }
  }

  fn has_class(&self, class: &CssString, case_sensitivity: CaseSensitivity) -> bool {
    if let Some(cache) = self.attr_cache {
      return cache.has_class(self.node, class.as_str(), case_sensitivity);
    }

    let is_html = self.is_html_element();
    let classes = self
      .node
      .attributes_iter()
      .find(|(name, _)| element_attr_cache_name_matches(name, "class", is_html))
      .map(|(_, value)| value);
    let Some(classes) = classes else {
      return false;
    };

    match case_sensitivity {
      CaseSensitivity::CaseSensitive => classes
        .split_ascii_whitespace()
        .any(|c| c == class.as_str()),
      CaseSensitivity::AsciiCaseInsensitive => classes
        .split_ascii_whitespace()
        .any(|c| c.eq_ignore_ascii_case(class.as_str())),
    }
  }

  fn imported_part(&self, name: &CssString) -> Option<CssString> {
    let Some(attr) = self.node.get_attribute_ref("exportparts") else {
      return None;
    };

    let target = name.as_str();
    for (internal, exported) in parse_exportparts(attr) {
      if exported == target {
        return Some(CssString::from(internal));
      }
    }

    None
  }

  fn is_part(&self, name: &CssString) -> bool {
    let Some(parts) = self.node.get_attribute_ref("part") else {
      return false;
    };

    let target = name.as_str();
    if parts.split_ascii_whitespace().any(|token| token == target) {
      return true;
    }

    // FastRender historically treated a shadow host's `exportparts` as renaming the parts exposed
    // to its containing scope (including the document). This isn't covered by the selectors crate's
    // built-in `::part()` matching, so we mirror the previous behavior here by treating exported
    // part aliases as additional part names on elements in that host's shadow tree.
    let Some(host) = self.containing_shadow_host() else {
      return false;
    };
    let Some(imported) = host.imported_part(name) else {
      return false;
    };
    parts
      .split_ascii_whitespace()
      .any(|token| token == imported.as_str())
  }

  fn is_empty(&self) -> bool {
    if self.node.is_template_element() {
      return true;
    }
    !self.node.children.iter().any(Self::subtree_has_content)
  }

  fn is_root(&self) -> bool {
    matches!(self.node.tag_name(), Some("html"))
  }

  fn first_element_child(&self) -> Option<Self> {
    for child in self.node.traversal_children() {
      if child.is_element() {
        return Some(ElementRef {
          node: child,
          node_id: 0,
          parent: Some(self.node),
          all_ancestors: self.all_ancestors,
          slot_map: self.slot_map,
          attr_cache: self.attr_cache,
        });
      }
    }
    None
  }

  fn apply_selector_flags(&self, _flags: selectors::matching::ElementSelectorFlags) {
    // We don't track selector flags for static rendering
  }

  fn has_custom_state(&self, _name: &CssString) -> bool {
    // We don't support custom states
    false
  }

  fn add_element_unique_hashes(
    &self,
    filter: &mut selectors::bloom::CountingBloomFilter<selectors::bloom::BloomStorageU8>,
  ) -> bool {
    if !selector_bloom_enabled() {
      return false;
    }

    if let Some(cache) = self.attr_cache {
      cache.for_each_selector_bloom_hash(self.node, |hash| filter.insert_hash(hash));
    } else {
      add_selector_bloom_hashes(self.node, &mut |hash| filter.insert_hash(hash));
    }
    true
  }
}

struct RelativeSelectorAncestorStack<'a> {
  baseline: &'a [&'a DomNode],
  nodes: Vec<&'a DomNode>,
  baseline_len: usize,
}

impl<'a> RelativeSelectorAncestorStack<'a> {
  fn new(baseline: &'a [&'a DomNode]) -> Self {
    let baseline_len = baseline.len();
    Self {
      baseline,
      nodes: Vec::new(),
      baseline_len,
    }
  }

  fn ensure_materialized(&mut self) {
    if self.nodes.is_empty() {
      self.nodes.reserve(self.baseline_len.saturating_add(8));
      self.nodes.extend_from_slice(self.baseline);
    }
  }

  fn as_slice(&self) -> &[&'a DomNode] {
    if self.nodes.is_empty() {
      self.baseline
    } else {
      &self.nodes
    }
  }

  fn parent(&self) -> Option<&'a DomNode> {
    if self.nodes.is_empty() {
      self.baseline.last().copied()
    } else {
      self.nodes.last().copied()
    }
  }

  fn push(&mut self, node: &'a DomNode) {
    self.ensure_materialized();
    self.nodes.push(node);
  }

  fn pop(&mut self) -> Option<&'a DomNode> {
    self.nodes.pop()
  }

  fn reset(&mut self) {
    if !self.nodes.is_empty() {
      self.nodes.truncate(self.baseline_len);
    }
  }

  fn len(&self) -> usize {
    if self.nodes.is_empty() {
      self.baseline_len
    } else {
      self.nodes.len()
    }
  }

  fn baseline_len(&self) -> usize {
    self.baseline_len
  }

  fn with_pushed<F, R>(&mut self, node: &'a DomNode, f: F) -> R
  where
    F: FnOnce(&mut Self) -> R,
  {
    self.push(node);
    let res = f(self);
    let popped = self.pop();
    debug_assert!(popped.is_some());
    res
  }
}

const RELATIVE_SELECTOR_BLOOM_HASHES_MAX: usize = 8;

fn append_relative_selector_quirks_id_class_hashes(
  selector: &Selector<FastRenderSelectorImpl>,
  out: &mut Vec<u32>,
) {
  use selectors::parser::Component;

  // Only consider the selector's rightmost compound. Inner selectors (e.g. within `:is()`) can
  // "break out" and match ancestors outside the :has() anchor subtree, so treating ancestor-side
  // class/id selectors as mandatory would lead to false negatives.
  for component in selector.iter() {
    match component {
      Component::ID(id) => out.push(selector_bloom_hash_ascii_lowercase(id.as_str())),
      Component::Class(class) => out.push(selector_bloom_hash_ascii_lowercase(class.as_str())),
      Component::Is(list) | Component::Where(list) => {
        let slice = list.slice();
        if slice.len() == 1 {
          append_relative_selector_quirks_id_class_hashes(&slice[0], out);
        }
      }
      _ => {}
    }

    if out.len() >= RELATIVE_SELECTOR_BLOOM_HASHES_MAX {
      break;
    }
  }
}

pub(crate) fn relative_selector_bloom_hashes(
  selector: &RelativeSelector<FastRenderSelectorImpl>,
  quirks_mode: QuirksMode,
) -> Vec<u32> {
  let mut hashes = selector.bloom_hashes.hashes_for_mode(quirks_mode).to_vec();
  if matches!(quirks_mode, QuirksMode::Quirks) && hashes.len() < RELATIVE_SELECTOR_BLOOM_HASHES_MAX
  {
    append_relative_selector_quirks_id_class_hashes(&selector.selector, &mut hashes);
    hashes.truncate(RELATIVE_SELECTOR_BLOOM_HASHES_MAX);
  }
  hashes
}

fn matches_has_relative(
  anchor: &ElementRef,
  selectors: &[RelativeSelector<FastRenderSelectorImpl>],
  context: &mut MatchingContext<FastRenderSelectorImpl>,
) -> bool {
  if selectors.is_empty() {
    return false;
  }

  context.nest_for_relative_selector(anchor.opaque(), |ctx| {
    ctx.nest_for_scope(Some(anchor.opaque()), |ctx| {
      let mut ancestors = RelativeSelectorAncestorStack::new(anchor.all_ancestors);
      let mut deadline_counter = 0usize;
      let mut use_ancestor_bloom = selector_bloom_enabled();
      let mut ancestor_bloom_baseline = BloomFilter::new();
      let mut ancestor_bloom_filter = BloomFilter::new();
      let anchor_id = if anchor.node_id != 0 {
        anchor.node_id
      } else {
        ctx.extra_data.node_id_for(anchor.node).unwrap_or(0)
      };
      let anchor_summary = ctx
        .extra_data
        .selector_blooms
        .and_then(|store| store.summary_for_id(anchor_id));

      if use_ancestor_bloom {
        if anchor.all_ancestors.len() > RELATIVE_SELECTOR_ANCESTOR_BLOOM_MAX_DEPTH {
          use_ancestor_bloom = false;
        } else if let Some(cache) = ctx.extra_data.element_attr_cache {
          for ancestor in anchor.all_ancestors {
            if let Err(err) = check_active_periodic(
              &mut deadline_counter,
              RELATIVE_SELECTOR_DEADLINE_STRIDE,
              RenderStage::Cascade,
            ) {
              ctx.extra_data.record_deadline_error(err);
              return false;
            }
            cache.for_each_selector_bloom_hash(ancestor, |hash| {
              ancestor_bloom_baseline.insert_hash(hash);
            });
          }
        } else {
          for ancestor in anchor.all_ancestors {
            if let Err(err) = check_active_periodic(
              &mut deadline_counter,
              RELATIVE_SELECTOR_DEADLINE_STRIDE,
              RenderStage::Cascade,
            ) {
              ctx.extra_data.record_deadline_error(err);
              return false;
            }
            add_selector_bloom_hashes(ancestor, &mut |hash| {
              ancestor_bloom_baseline.insert_hash(hash);
            });
          }
        }
      }

      for selector in selectors.iter() {
        record_has_eval();
        if let Err(err) = check_active_periodic(
          &mut deadline_counter,
          RELATIVE_SELECTOR_DEADLINE_STRIDE,
          RenderStage::Cascade,
        ) {
          ctx.extra_data.record_deadline_error(err);
          return false;
        }
        if let Some(cached) = ctx
          .selector_caches
          .relative_selector
          .lookup(anchor.opaque(), selector)
        {
          record_has_cache_hit();
          if cached.matched() {
            return true;
          }
          continue;
        }

        let quirks_mode = ctx.quirks_mode();
         if selector.match_hint.is_descendant_direction() {
           if let Some(summary) = anchor_summary {
             let should_prune = if matches!(quirks_mode, QuirksMode::Quirks) {
               let hashes = relative_selector_bloom_hashes(selector, quirks_mode);
               !hashes.is_empty() && hashes.iter().any(|hash| !summary.contains_hash(*hash))
             } else {
               let hashes = selector.bloom_hashes.hashes_for_mode(quirks_mode);
               !hashes.is_empty() && hashes.iter().any(|hash| !summary.contains_hash(*hash))
             };

             if should_prune {
               record_has_prune();
               ctx.selector_caches.relative_selector.add(
                 anchor.opaque(),
                 selector,
                 RelativeSelectorCachedMatch::NotMatched,
               );
               continue;
             }
           }
         }

         if !selector.match_hint.is_descendant_direction() {
           let parent = match anchor.parent {
             Some(parent) => parent,
             None => {
               record_has_prune();
               ctx.selector_caches.relative_selector.add(
                 anchor.opaque(),
                 selector,
                 RelativeSelectorCachedMatch::NotMatched,
               );
               continue;
             }
           };
           if parent.template_contents_are_inert() {
             record_has_prune();
             ctx.selector_caches.relative_selector.add(
               anchor.opaque(),
               selector,
               RelativeSelectorCachedMatch::NotMatched,
             );
             continue;
           }
 
           let mut seen_anchor = false;
           let mut next_sibling: Option<&DomNode> = None;
           for sibling in parent
             .traversal_children()
             .iter()
             .filter(|c| c.is_element())
           {
             if ptr::eq(sibling, anchor.node) {
               seen_anchor = true;
               continue;
             }
             if !seen_anchor {
               continue;
             }
             next_sibling = Some(sibling);
             break;
           }
 
            let Some(next_sibling) = next_sibling else {
              record_has_prune();
              ctx.selector_caches.relative_selector.add(
                anchor.opaque(),
                selector,
                RelativeSelectorCachedMatch::NotMatched,
              );
              continue;
            };
  
            if selector.match_hint.is_next_sibling() {
              if let Some(store) = ctx.extra_data.selector_blooms {
                let sibling_id = ctx
                  .extra_data
                  .node_id_for(next_sibling)
                  .or_else(|| ctx.extra_data.slot_map.and_then(|map| map.node_id(next_sibling)));
                if let Some(sibling_id) = sibling_id {
                  if let Some(summary) = store.summary_for_id(sibling_id) {
                    let should_prune = if matches!(quirks_mode, QuirksMode::Quirks) {
                      let hashes = relative_selector_bloom_hashes(selector, quirks_mode);
                      !hashes.is_empty() && hashes.iter().any(|hash| !summary.contains_hash(*hash))
                    } else {
                      let hashes = selector.bloom_hashes.hashes_for_mode(quirks_mode);
                      !hashes.is_empty() && hashes.iter().any(|hash| !summary.contains_hash(*hash))
                    };
                    if should_prune {
                      record_has_prune();
                      ctx.selector_caches.relative_selector.add(
                        anchor.opaque(),
                        selector,
                        RelativeSelectorCachedMatch::NotMatched,
                      );
                      continue;
                    }
                  }
                }
              }
            }
          }
 
         if selector_bloom_enabled()
           && ctx
             .selector_caches
             .relative_selector_filter_map
            .fast_reject(anchor, selector, ctx.quirks_mode())
        {
          if ctx.extra_data.deadline_error.is_some() {
            return false;
          }
          record_has_prune();
          record_has_filter_prune();
          ctx.selector_caches.relative_selector.add(
            anchor.opaque(),
            selector,
            RelativeSelectorCachedMatch::NotMatched,
          );
          continue;
        }

        if use_ancestor_bloom {
          ancestor_bloom_filter = ancestor_bloom_baseline.clone();
        }

        record_has_relative_eval();
        let matched = match_relative_selector(
          selector,
          anchor.node,
          &mut ancestors,
          &mut ancestor_bloom_filter,
          use_ancestor_bloom,
          ctx,
          &mut deadline_counter,
        );
        debug_assert_eq!(ancestors.len(), ancestors.baseline_len());
        ancestors.reset();

        if ctx.extra_data.deadline_error.is_some() {
          return false;
        }

        ctx.selector_caches.relative_selector.add(
          anchor.opaque(),
          selector,
          if matched {
            RelativeSelectorCachedMatch::Matched
          } else {
            RelativeSelectorCachedMatch::NotMatched
          },
        );

        if matched {
          return true;
        }
      }

      false
    })
  })
}

// RelativeSelectorMatchHint guides traversal here:
// - is_descendant_direction() => combinators keep moving downward (descendant or child),
//   otherwise we only need to consider following siblings.
// - is_subtree() => a match may occur inside the candidate's subtree, so we recurse;
//   when false we only try the candidate itself.
// - is_next_sibling() => only the immediate following sibling can match.
fn match_relative_selector<'a>(
  selector: &RelativeSelector<FastRenderSelectorImpl>,
  anchor: &'a DomNode,
  ancestors: &mut RelativeSelectorAncestorStack<'a>,
  bloom_filter: &mut BloomFilter,
  use_ancestor_bloom: bool,
  context: &mut MatchingContext<FastRenderSelectorImpl>,
  deadline_counter: &mut usize,
) -> bool {
  if !anchor.is_element() {
    return false;
  }

  if selector.match_hint.is_descendant_direction() {
    return match_relative_selector_descendants(
      selector,
      anchor,
      ancestors,
      bloom_filter,
      use_ancestor_bloom,
      context,
      deadline_counter,
    );
  }

  match_relative_selector_siblings(
    selector,
    anchor,
    ancestors,
    bloom_filter,
    use_ancestor_bloom,
    context,
    deadline_counter,
  )
}

fn in_shadow_tree(ancestors: &[&DomNode]) -> bool {
  ancestors
    .iter()
    .any(|node| matches!(node.node_type, DomNodeType::ShadowRoot { .. }))
}

fn shadow_root_child(node: &DomNode) -> Option<&DomNode> {
  node
    .traversal_children()
    .iter()
    .find(|child| matches!(child.node_type, DomNodeType::ShadowRoot { .. }))
}

fn has_relative_anchor_can_traverse_shadow_root(
  anchor: &DomNode,
  context: &MatchingContext<FastRenderSelectorImpl>,
) -> bool {
  context
    .extra_data
    .shadow_host
    .is_some_and(|host| host == OpaqueElement::new(anchor))
}

fn for_each_assigned_slot_child<'a, F: FnMut(&'a DomNode)>(node: &'a DomNode, f: &mut F) {
  // Avoid recursion for degenerate trees (can be reached when traversing shadow roots that contain
  // nested slots).
  let mut stack: Vec<&'a DomNode> = Vec::new();
  for child in node.traversal_children().iter().rev() {
    stack.push(child);
  }

  while let Some(node) = stack.pop() {
    match &node.node_type {
      DomNodeType::Slot { assigned: true, .. } => {
        for assigned_child in node.traversal_children().iter().filter(|c| c.is_element()) {
          f(assigned_child);
        }
      }
      DomNodeType::ShadowRoot { .. } => {}
      _ => {
        for child in node.traversal_children().iter().rev() {
          stack.push(child);
        }
      }
    }
  }
}

fn for_each_selector_child<'a, F: FnMut(&'a DomNode)>(
  anchor: &'a DomNode,
  ancestors: &[&'a DomNode],
  mut f: F,
) {
  let within_shadow_tree = in_shadow_tree(ancestors);
  for child in anchor.traversal_children() {
    match &child.node_type {
      DomNodeType::ShadowRoot { .. } => {
        if within_shadow_tree {
          continue;
        }
        for_each_assigned_slot_child(child, &mut f);
      }
      _ => {
        if child.is_element() {
          f(child);
        }
      }
    }
  }
}

fn match_relative_selector_descendants<'a>(
  selector: &RelativeSelector<FastRenderSelectorImpl>,
  anchor: &'a DomNode,
  ancestors: &mut RelativeSelectorAncestorStack<'a>,
  bloom_filter: &mut BloomFilter,
  use_ancestor_bloom: bool,
  context: &mut MatchingContext<FastRenderSelectorImpl>,
  deadline_counter: &mut usize,
) -> bool {
  // HTML template contents are not part of the DOM tree for selector matching; do not traverse into
  // them. The <template> element itself is still matchable via its parent.
  if anchor.template_contents_are_inert() {
    return false;
  }
  let traverse_shadow_root = has_relative_anchor_can_traverse_shadow_root(anchor, context);
  let ancestor_hashes = selector.ancestor_hashes_for_mode(context.quirks_mode());
  ancestors.with_pushed(anchor, |ancestors| {
    if use_ancestor_bloom {
      if let Some(cache) = context.extra_data.element_attr_cache {
        cache.for_each_selector_bloom_hash(anchor, |hash| bloom_filter.insert_hash(hash));
      } else {
        add_selector_bloom_hashes(anchor, &mut |hash| bloom_filter.insert_hash(hash));
      }
    }
    let mut found = false;
    if traverse_shadow_root {
      if let Some(shadow_root) = shadow_root_child(anchor) {
        ancestors.with_pushed(shadow_root, |ancestors| {
          for child in shadow_root
            .traversal_children()
            .iter()
            .filter(|c| c.is_element())
          {
            if let Err(err) = check_active_periodic(
              deadline_counter,
              RELATIVE_SELECTOR_DEADLINE_STRIDE,
              RenderStage::Cascade,
            ) {
              context.extra_data.record_deadline_error(err);
              found = false;
              break;
            }
            let child_ref = ElementRef::with_ancestors(child, ancestors.as_slice())
              .with_node_id(context.extra_data.node_id_for(child).unwrap_or(0))
              .with_slot_map(context.extra_data.slot_map)
              .with_attr_cache(context.extra_data.element_attr_cache);
            let mut matched =
              if use_ancestor_bloom && !selector_may_match(ancestor_hashes, bloom_filter) {
                false
              } else {
                matches_selector(&selector.selector, 0, None, &child_ref, context)
              };
            if context.extra_data.deadline_error.is_some() {
              found = false;
              break;
            }
            if !matched && selector.match_hint.is_subtree() {
              matched = match_relative_selector_subtree(
                selector,
                child,
                ancestors,
                bloom_filter,
                use_ancestor_bloom,
                context,
                deadline_counter,
              );
            }
            if matched {
              found = true;
              break;
            }
          }
        })
      }
    } else {
      for child in anchor
        .traversal_children()
        .iter()
        .filter(|c| c.is_element())
      {
        if let Err(err) = check_active_periodic(
          deadline_counter,
          RELATIVE_SELECTOR_DEADLINE_STRIDE,
          RenderStage::Cascade,
        ) {
          context.extra_data.record_deadline_error(err);
          found = false;
          break;
        }
        let child_ref = ElementRef::with_ancestors(child, ancestors.as_slice())
          .with_node_id(context.extra_data.node_id_for(child).unwrap_or(0))
          .with_slot_map(context.extra_data.slot_map)
          .with_attr_cache(context.extra_data.element_attr_cache);
        let mut matched =
          if use_ancestor_bloom && !selector_may_match(ancestor_hashes, bloom_filter) {
            false
          } else {
            matches_selector(&selector.selector, 0, None, &child_ref, context)
          };
        if context.extra_data.deadline_error.is_some() {
          found = false;
          break;
        }
        if !matched && selector.match_hint.is_subtree() {
          matched = match_relative_selector_subtree(
            selector,
            child,
            ancestors,
            bloom_filter,
            use_ancestor_bloom,
            context,
            deadline_counter,
          );
        }
        if matched {
          found = true;
          break;
        }
      }
    }
    if use_ancestor_bloom {
      if let Some(cache) = context.extra_data.element_attr_cache {
        cache.for_each_selector_bloom_hash(anchor, |hash| bloom_filter.remove_hash(hash));
      } else {
        add_selector_bloom_hashes(anchor, &mut |hash| bloom_filter.remove_hash(hash));
      }
    }
    found
  })
}

fn match_relative_selector_siblings<'a>(
  selector: &RelativeSelector<FastRenderSelectorImpl>,
  anchor: &'a DomNode,
  ancestors: &mut RelativeSelectorAncestorStack<'a>,
  bloom_filter: &mut BloomFilter,
  use_ancestor_bloom: bool,
  context: &mut MatchingContext<FastRenderSelectorImpl>,
  deadline_counter: &mut usize,
) -> bool {
  if has_relative_anchor_can_traverse_shadow_root(anchor, context) {
    // When matching in a shadow tree, the host replaces the shadow root node and becomes the root
    // of the selector tree. Relative selectors starting with sibling combinators therefore cannot
    // match anything when anchored on the shadow host.
    return false;
  }
  let ancestor_hashes = selector.ancestor_hashes_for_mode(context.quirks_mode());
  let quirks_mode = context.quirks_mode();
  let selector_blooms = context.extra_data.selector_blooms;
  let node_to_id = context.extra_data.node_to_id;
  let slot_map = context.extra_data.slot_map;
  let hashes_quirks = (selector_blooms.is_some()
    && (node_to_id.is_some() || slot_map.is_some())
    && matches!(quirks_mode, QuirksMode::Quirks))
  .then(|| relative_selector_bloom_hashes(selector, quirks_mode));
  let hashes: &[u32] = if let Some(hashes) = hashes_quirks.as_deref() {
    hashes
  } else if selector_blooms.is_some() && (node_to_id.is_some() || slot_map.is_some()) {
    selector.bloom_hashes.hashes_for_mode(quirks_mode)
  } else {
    &[]
  };
  let parent = match ancestors.parent() {
    Some(p) => p,
    None => return false,
  };
  if parent.template_contents_are_inert() {
    return false;
  }

  let mut seen_anchor = false;
  for sibling in parent
    .traversal_children()
    .iter()
    .filter(|c| c.is_element())
  {
    if let Err(err) = check_active_periodic(
      deadline_counter,
      RELATIVE_SELECTOR_DEADLINE_STRIDE,
      RenderStage::Cascade,
    ) {
      context.extra_data.record_deadline_error(err);
      return false;
    }
    if ptr::eq(sibling, anchor) {
      seen_anchor = true;
      continue;
    }
    if !seen_anchor {
      continue;
    }

    if !hashes.is_empty() {
      let sibling_id = context
        .extra_data
        .node_id_for(sibling)
        .or_else(|| slot_map.and_then(|map| map.node_id(sibling)));
      if let (Some(store), Some(sibling_id)) = (selector_blooms, sibling_id) {
        if let Some(summary) = store.summary_for_id(sibling_id) {
          if hashes.iter().any(|hash| !summary.contains_hash(*hash)) {
            if selector.match_hint.is_next_sibling() {
              break;
            }
            continue;
          }
        }
      }
    }

    let sibling_ref = ElementRef::with_ancestors(sibling, ancestors.as_slice())
      .with_node_id(context.extra_data.node_id_for(sibling).unwrap_or(0))
      .with_slot_map(context.extra_data.slot_map)
      .with_attr_cache(context.extra_data.element_attr_cache);
    let matched = if selector.match_hint.is_subtree() {
      match_relative_selector_subtree(
        selector,
        sibling,
        ancestors,
        bloom_filter,
        use_ancestor_bloom,
        context,
        deadline_counter,
      )
    } else {
      if use_ancestor_bloom && !selector_may_match(ancestor_hashes, bloom_filter) {
        false
      } else {
        matches_selector(&selector.selector, 0, None, &sibling_ref, context)
      }
    };
    if context.extra_data.deadline_error.is_some() {
      return false;
    }

    if matched {
      return true;
    }

    if selector.match_hint.is_next_sibling() {
      break;
    }
  }

  false
}

fn match_relative_selector_subtree<'a>(
  selector: &RelativeSelector<FastRenderSelectorImpl>,
  node: &'a DomNode,
  ancestors: &mut RelativeSelectorAncestorStack<'a>,
  bloom_filter: &mut BloomFilter,
  use_ancestor_bloom: bool,
  context: &mut MatchingContext<FastRenderSelectorImpl>,
  deadline_counter: &mut usize,
) -> bool {
  debug_assert!(selector.match_hint.is_subtree());
  // HTML template contents are inert and should not be traversed for selector matching.
  if node.template_contents_are_inert() {
    return false;
  }

  // The u8-backed counting bloom filter saturates at 0xff and cannot be decremented once
  // saturated to avoid false negatives. On extremely deep trees this can prevent the filter
  // from returning to the all-zero state, so cap ancestor bloom usage and fall back to full
  // selector matching beyond that depth.

  let ancestor_hashes = selector.ancestor_hashes_for_mode(context.quirks_mode());
  let element_attr_cache = context.extra_data.element_attr_cache;
  let slot_map = context.extra_data.slot_map;
  let relative_anchor = context.relative_selector_anchor();
  let has_shadow_root_anchor = matches!(
    (context.extra_data.shadow_host, relative_anchor),
    (Some(host), Some(anchor)) if host == anchor
  );

  fn push_bloom(
    node: &DomNode,
    bloom_filter: &mut BloomFilter,
    use_ancestor_bloom: bool,
    element_attr_cache: Option<&ElementAttrCache>,
  ) {
    if !use_ancestor_bloom || !node.is_element() {
      return;
    }
    if let Some(cache) = element_attr_cache {
      cache.for_each_selector_bloom_hash(node, |hash| bloom_filter.insert_hash(hash));
    } else {
      add_selector_bloom_hashes(node, &mut |hash| bloom_filter.insert_hash(hash));
    }
  }

  fn pop_bloom(
    node: &DomNode,
    bloom_filter: &mut BloomFilter,
    use_ancestor_bloom: bool,
    element_attr_cache: Option<&ElementAttrCache>,
  ) {
    if !use_ancestor_bloom || !node.is_element() {
      return;
    }
    if let Some(cache) = element_attr_cache {
      cache.for_each_selector_bloom_hash(node, |hash| bloom_filter.remove_hash(hash));
    } else {
      add_selector_bloom_hashes(node, &mut |hash| bloom_filter.remove_hash(hash));
    }
  }

  struct Frame<'a> {
    node: &'a DomNode,
    next_child: usize,
    bloomed: bool,
  }

  ancestors.push(node);
  let root_bloomed = use_ancestor_bloom && RELATIVE_SELECTOR_ANCESTOR_BLOOM_MAX_DEPTH > 0;
  push_bloom(node, bloom_filter, root_bloomed, element_attr_cache);
  let mut stack: Vec<Frame<'a>> = Vec::new();
  stack.push(Frame {
    node,
    next_child: 0,
    bloomed: root_bloomed,
  });

  let mut result = false;
  loop {
    let frame = match stack.last_mut() {
      Some(frame) => frame,
      None => break,
    };

    let children = frame.node.traversal_children();
    let mut child_is_shadow_root = false;
    let mut next_child: Option<&DomNode> = None;
    while frame.next_child < children.len() {
      let candidate = &children[frame.next_child];
      frame.next_child += 1;

      if candidate.is_element() {
        if has_shadow_root_anchor
          && relative_anchor.is_some_and(|anchor| anchor == OpaqueElement::new(frame.node))
        {
          // In a shadow-tree selector context, the host's light-DOM children are outside the
          // selector tree and must not participate in :has() relative selector matching.
          continue;
        }
        next_child = Some(candidate);
        break;
      }

      if has_shadow_root_anchor
        && relative_anchor.is_some_and(|anchor| anchor == OpaqueElement::new(frame.node))
        && matches!(candidate.node_type, DomNodeType::ShadowRoot { .. })
      {
        child_is_shadow_root = true;
        next_child = Some(candidate);
        break;
      }
    }

    let Some(child) = next_child else {
      let Some(finished) = stack.pop() else {
        // Invariant violation: `frame` came from `stack.last_mut()`, so `stack` must be non-empty.
        // Treat as non-match and reset any local traversal state so future selector evaluations
        // start from a known-good baseline.
        result = false;
        ancestors.reset();
        *bloom_filter = BloomFilter::new();
        break;
      };
      if finished.bloomed {
        pop_bloom(
          finished.node,
          bloom_filter,
          finished.bloomed,
          element_attr_cache,
        );
      }
      let popped = ancestors.pop();
      debug_assert!(popped.is_some());
      continue;
    };

    if let Err(err) = check_active_periodic(
      deadline_counter,
      RELATIVE_SELECTOR_DEADLINE_STRIDE,
      RenderStage::Cascade,
    ) {
      context.extra_data.record_deadline_error(err);
      result = false;
      break;
    }

    if !child_is_shadow_root {
      let child_ref = ElementRef::with_ancestors(child, ancestors.as_slice())
        .with_node_id(context.extra_data.node_id_for(child).unwrap_or(0))
        .with_slot_map(slot_map)
        .with_attr_cache(element_attr_cache);

      let bloom_active =
        use_ancestor_bloom && stack.len() <= RELATIVE_SELECTOR_ANCESTOR_BLOOM_MAX_DEPTH;
      let may_match = !bloom_active || selector_may_match(ancestor_hashes, bloom_filter);
      if may_match && matches_selector(&selector.selector, 0, None, &child_ref, context) {
        if context.extra_data.deadline_error.is_some() {
          result = false;
          break;
        }
        result = true;
        break;
      }
      if context.extra_data.deadline_error.is_some() {
        result = false;
        break;
      }
    }
    if context.extra_data.deadline_error.is_some() {
      result = false;
      break;
    }

    // Do not recurse into HTML template contents.
    if child.template_contents_are_inert() {
      continue;
    }

    ancestors.push(child);
    let child_bloomed = use_ancestor_bloom
      && child.is_element()
      && stack.len() < RELATIVE_SELECTOR_ANCESTOR_BLOOM_MAX_DEPTH;
    push_bloom(child, bloom_filter, child_bloomed, element_attr_cache);
    stack.push(Frame {
      node: child,
      next_child: 0,
      bloomed: child_bloomed,
    });
  }

  while let Some(frame) = stack.pop() {
    if frame.bloomed {
      pop_bloom(frame.node, bloom_filter, frame.bloomed, element_attr_cache);
    }
    let popped = ancestors.pop();
    debug_assert!(popped.is_some());
  }

  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::selectors::build_relative_selectors;
  use crate::css::selectors::PseudoClassParser;
  use crate::css::selectors::ShadowMatchData;
  use crate::render_control::{with_deadline, RenderDeadline};
  use cssparser::{Parser, ParserInput};
  use selectors::context::QuirksMode;
  use selectors::matching::MatchingContext;
  use selectors::matching::MatchingForInvalidation;
  use selectors::matching::MatchingMode;
  use selectors::matching::NeedsSelectorFlags;
  use selectors::matching::SelectorCaches;
  use selectors::parser::ParseRelative;
  use selectors::parser::Selector;
  use selectors::parser::SelectorList;

  fn element(tag: &str, children: Vec<DomNode>) -> DomNode {
    DomNode {
      node_type: DomNodeType::Element {
        tag_name: tag.to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children,
    }
  }

  fn element_with_attrs(tag: &str, attrs: Vec<(&str, &str)>, children: Vec<DomNode>) -> DomNode {
    DomNode {
      node_type: DomNodeType::Element {
        tag_name: tag.to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: attrs
          .into_iter()
          .map(|(k, v)| (k.to_string(), v.to_string()))
          .collect(),
      },
      children,
    }
  }

  fn document(children: Vec<DomNode>) -> DomNode {
    DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children,
    }
  }

  fn svg_element(tag: &str) -> DomNode {
    DomNode {
      node_type: DomNodeType::Element {
        tag_name: tag.to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    }
  }

  fn text(content: &str) -> DomNode {
    DomNode {
      node_type: DomNodeType::Text {
        content: content.to_string(),
      },
      children: vec![],
    }
  }

  #[test]
  fn textarea_value_normalizes_newlines() {
    let textarea = element("textarea", vec![text("a\r\nb\rc")]);
    assert_eq!(textarea_value(&textarea), "a\nb\nc");
  }

  #[test]
  fn textarea_value_strips_single_leading_newline() {
    let textarea = element("textarea", vec![text("\nhello")]);
    assert_eq!(textarea_value(&textarea), "hello");
  }

  #[test]
  fn textarea_value_does_not_trim_whitespace() {
    let textarea = element("textarea", vec![text(" ")]);
    assert_eq!(textarea_value(&textarea), " ");
  }

  #[test]
  fn input_range_bounds_defaults_and_collapses_invalid_range() {
    let default_bounds = element_with_attrs("input", vec![("type", "range")], vec![]);
    assert_eq!(input_range_bounds(&default_bounds), Some((0.0, 100.0)));

    let invalid = element_with_attrs(
      "input",
      vec![("type", "range"), ("min", "nope"), ("max", "nan")],
      vec![],
    );
    assert_eq!(input_range_bounds(&invalid), Some((0.0, 100.0)));

    let reversed = element_with_attrs(
      "input",
      vec![("type", "range"), ("min", "10"), ("max", "5")],
      vec![],
    );
    assert_eq!(input_range_bounds(&reversed), Some((10.0, 10.0)));
  }

  #[test]
  fn input_range_value_defaults_clamps_and_snaps() {
    let default_midpoint = element_with_attrs(
      "input",
      vec![("type", "range"), ("min", "0"), ("max", "10")],
      vec![],
    );
    assert_eq!(input_range_value(&default_midpoint), Some(5.0));

    let invalid_value = element_with_attrs(
      "input",
      vec![("type", "range"), ("min", "0"), ("max", "10"), ("value", "oops")],
      vec![],
    );
    assert_eq!(input_range_value(&invalid_value), Some(5.0));

    let clamped = element_with_attrs(
      "input",
      vec![("type", "range"), ("min", "0"), ("max", "10"), ("value", "20")],
      vec![],
    );
    assert_eq!(input_range_value(&clamped), Some(10.0));

    let step_any = element_with_attrs(
      "input",
      vec![
        ("type", "range"),
        ("min", "0"),
        ("max", "10"),
        ("value", "3.3"),
        ("step", "any"),
      ],
      vec![],
    );
    let got = input_range_value(&step_any).expect("step any value");
    assert!((got - 3.3).abs() < 1e-9, "expected 3.3, got {got}");

    let snapped = element_with_attrs(
      "input",
      vec![
        ("type", "range"),
        ("min", "0"),
        ("max", "10"),
        ("value", "10"),
        ("step", "4"),
      ],
      vec![],
    );
    assert_eq!(input_range_value(&snapped), Some(8.0));
  }

  #[test]
  fn input_color_value_string_sanitizes_to_simple_color_or_default() {
    let missing = element_with_attrs("input", vec![("type", "color")], vec![]);
    assert_eq!(
      input_color_value_string(&missing).as_deref(),
      Some("#000000"),
      "color inputs default to black when no value is provided"
    );

    let valid = element_with_attrs(
      "input",
      vec![("type", "color"), ("value", "#00FF00")],
      vec![],
    );
    assert_eq!(
      input_color_value_string(&valid).as_deref(),
      Some("#00ff00"),
      "simple colors should be accepted and normalized to lowercase"
    );

    let invalid_name = element_with_attrs(
      "input",
      vec![("type", "color"), ("value", "red")],
      vec![],
    );
    assert_eq!(
      input_color_value_string(&invalid_name).as_deref(),
      Some("#000000"),
      "named colors are not valid simple colors for <input type=color>"
    );

    let invalid_shorthand = element_with_attrs(
      "input",
      vec![("type", "color"), ("value", "#f60")],
      vec![],
    );
    assert_eq!(
      input_color_value_string(&invalid_shorthand).as_deref(),
      Some("#000000"),
      "shorthand hex colors are not valid simple colors for <input type=color>"
    );
  }

  #[test]
  fn non_ascii_whitespace_input_range_bounds_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let min = format!("{nbsp}10");
    let node = element_with_attrs(
      "input",
      vec![("type", "range"), ("min", min.as_str()), ("max", "20")],
      vec![],
    );
    assert_eq!(
      input_range_bounds(&node),
      Some((0.0, 20.0)),
      "NBSP must not be treated as whitespace when parsing range bounds"
    );
  }

  #[test]
  fn non_ascii_whitespace_input_range_step_any_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let step = format!("{nbsp}any");
    let node = element_with_attrs(
      "input",
      vec![
        ("type", "range"),
        ("min", "0"),
        ("max", "10"),
        ("value", "3.3"),
        ("step", step.as_str()),
      ],
      vec![],
    );
    assert_eq!(
      input_range_value(&node),
      Some(3.0),
      "NBSP must not be treated as whitespace when matching step=any"
    );
  }

  #[test]
  fn required_whitespace_is_not_value_missing() {
    let textarea = element_with_attrs("textarea", vec![("required", "")], vec![text(" ")]);
    assert!(ElementRef::new(&textarea).accessibility_is_valid());

    let input = element_with_attrs("input", vec![("required", ""), ("value", " ")], vec![]);
    assert!(ElementRef::new(&input).accessibility_is_valid());
  }

  #[test]
  fn non_ascii_whitespace_select_display_size_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let size = format!("{nbsp}2");
    let select = element_with_attrs("select", vec![("size", size.as_str())], vec![]);
    assert_eq!(
      select_display_size(&select),
      1,
      "NBSP must not be treated as whitespace when parsing <select size>"
    );
  }

  #[test]
  fn single_select_falls_back_to_first_option_when_all_disabled() {
    let select = element(
      "select",
      vec![
        element_with_attrs(
          "option",
          vec![("disabled", ""), ("value", "a")],
          vec![text("First")],
        ),
        element_with_attrs(
          "option",
          vec![("disabled", ""), ("value", "b")],
          vec![text("Second")],
        ),
      ],
    );

    let first = &select.children[0];
    let second = &select.children[1];
    let ancestors = [&select];
    assert!(ElementRef::with_ancestors(first, &ancestors).is_option_selected());
    assert!(!ElementRef::with_ancestors(second, &ancestors).is_option_selected());

    assert_eq!(ElementRef::new(&select).select_value().as_deref(), Some("a"));
  }

  #[test]
  fn disabled_selected_option_remains_selected_in_single_select() {
    let select = element_with_attrs(
      "select",
      vec![("required", "")],
      vec![
        element_with_attrs(
          "option",
          vec![("selected", ""), ("disabled", ""), ("value", "")],
          vec![text("Choose")],
        ),
        element_with_attrs("option", vec![("value", "x")], vec![text("X")]),
      ],
    );

    let placeholder = &select.children[0];
    let enabled = &select.children[1];
    let ancestors = [&select];
    assert!(ElementRef::with_ancestors(placeholder, &ancestors).is_option_selected());
    assert!(!ElementRef::with_ancestors(enabled, &ancestors).is_option_selected());

    assert_eq!(ElementRef::new(&select).select_value().as_deref(), Some(""));
    assert!(!ElementRef::new(&select).accessibility_is_valid());
  }

  #[test]
  fn select_size_parsing_treats_zero_and_invalid_as_absent() {
    let size0 = element_with_attrs("select", vec![("size", "0")], vec![]);
    assert_eq!(parse_select_size_attribute(&size0), None);
    assert!(!select_is_listbox(&size0));
    assert_eq!(select_effective_size(&size0), 1);

    let negative = element_with_attrs("select", vec![("size", "-3")], vec![]);
    assert_eq!(parse_select_size_attribute(&negative), None);
    assert!(!select_is_listbox(&negative));

    let invalid = element_with_attrs("select", vec![("size", "abc")], vec![]);
    assert_eq!(parse_select_size_attribute(&invalid), None);
    assert!(!select_is_listbox(&invalid));

    let multi_default = element_with_attrs("select", vec![("multiple", "")], vec![]);
    assert!(select_is_listbox(&multi_default));
    assert_eq!(select_effective_size(&multi_default), 4);

    let multi_size0 =
      element_with_attrs("select", vec![("multiple", ""), ("size", "0")], vec![]);
    assert!(select_is_listbox(&multi_size0));
    assert_eq!(select_effective_size(&multi_size0), 4);

    let multi_size3 =
      element_with_attrs("select", vec![("multiple", ""), ("size", "3")], vec![]);
    assert!(select_is_listbox(&multi_size3));
    assert_eq!(select_effective_size(&multi_size3), 3);
  }

  #[test]
  fn img_src_is_placeholder_accepts_unpadded_base64_data_url() {
    let padded = "data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==";
    assert!(img_src_is_placeholder(padded));

    let unpadded = "data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw";
    assert!(img_src_is_placeholder(unpadded));
  }

  #[test]
  fn img_src_is_placeholder_accepts_base64_data_url_with_whitespace() {
    let url = "data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEK\nAAEALAAAAAABAAEAAAICTAEAOw==";
    assert!(img_src_is_placeholder(url));
  }

  #[test]
  fn img_src_is_placeholder_accepts_fragment_and_script_urls() {
    assert!(img_src_is_placeholder("#"));
    assert!(img_src_is_placeholder("#foo"));
    assert!(img_src_is_placeholder("about:blank#foo"));
    assert!(img_src_is_placeholder("javascript:void(0)"));
    assert!(img_src_is_placeholder("vbscript:msgbox(\"x\")"));
    assert!(img_src_is_placeholder("mailto:test@example.com"));
  }

  fn enumerate_dom_ids_legacy(root: &DomNode) -> HashMap<*const DomNode, usize> {
    fn walk(node: &DomNode, next: &mut usize, map: &mut HashMap<*const DomNode, usize>) {
      map.insert(node as *const DomNode, *next);
      *next += 1;
      for child in node.children.iter() {
        walk(child, next, map);
      }
    }

    let mut ids: HashMap<*const DomNode, usize> = HashMap::new();
    let mut next_id = 1usize;
    walk(root, &mut next_id, &mut ids);
    ids
  }

  #[test]
  fn selector_bloom_hash_matches_selector_token_hash() {
    use precomputed_hash::PrecomputedHash;

    let value = "data-Thing";
    let token_hash = CssString::from(value).precomputed_hash() & selectors::bloom::BLOOM_HASH_MASK;
    let dom_hash = selector_bloom_hash(value);
    assert_eq!(dom_hash, token_hash);
  }

  fn find_element_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
    if let DomNodeType::Element { attributes, .. } = &node.node_type {
      if attributes
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == id)
      {
        return Some(node);
      }
    }
    for child in node.children.iter() {
      if let Some(found) = find_element_by_id(child, id) {
        return Some(found);
      }
    }
    None
  }

  fn find_node_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
    if node.get_attribute_ref("id") == Some(id) {
      return Some(node);
    }
    for child in node.children.iter() {
      if let Some(found) = find_node_by_id(child, id) {
        return Some(found);
      }
    }
    None
  }

  fn contains_shadow_root(node: &DomNode) -> bool {
    if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
      return true;
    }
    node.children.iter().any(contains_shadow_root)
  }

  #[test]
  fn dom_parse_timeout_is_cooperative() {
    let deadline = RenderDeadline::new(Some(std::time::Duration::from_millis(0)), None);
    let result = with_deadline(Some(&deadline), || parse_html("<div>hello</div>"));

    match result {
      Err(Error::Render(crate::error::RenderError::Timeout { stage, .. })) => {
        assert_eq!(stage, RenderStage::DomParse);
      }
      other => panic!("expected dom_parse timeout, got {other:?}"),
    }
  }

  #[test]
  fn dom_clone_timeout_is_cooperative() {
    let mut dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![],
    };
    let mut current = &mut dom;
    for _ in 0..(super::DOM_PARSE_NODE_DEADLINE_STRIDE * 2) {
      current.children.push(DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      });
      current = current.children.last_mut().expect("child pushed");
    }

    let deadline = RenderDeadline::new(Some(std::time::Duration::from_millis(0)), None);
    let result = with_deadline(Some(&deadline), || {
      clone_dom_with_deadline(&dom, RenderStage::DomParse)
    });

    match result {
      Err(Error::Render(crate::error::RenderError::Timeout { stage, .. })) => {
        assert_eq!(stage, RenderStage::DomParse);
      }
      other => panic!("expected dom_parse timeout during clone, got {other:?}"),
    }
  }

  #[test]
  fn parse_html_empty_input_does_not_panic() {
    let result = std::panic::catch_unwind(|| parse_html(""));
    assert!(result.is_ok(), "parse_html panicked on empty input");
    assert!(
      result.unwrap().is_ok(),
      "parse_html returned error on empty input"
    );
  }

  #[test]
  fn parse_html_broken_markup_does_not_panic() {
    let html = "<div><span></div><p><b><i>unclosed";
    let result = std::panic::catch_unwind(|| parse_html(html));
    assert!(result.is_ok(), "parse_html panicked on malformed input");
    assert!(
      result.unwrap().is_ok(),
      "parse_html returned error on malformed input"
    );
  }

  #[test]
  fn parse_html_deeply_nested_markup_does_not_panic() {
    // Keep this large enough to exercise the non-recursive DOM conversion/drop paths without
    // turning the unit test suite into a stress benchmark.
    const DEPTH: usize = 20_000;
    let mut html = String::with_capacity(DEPTH * 11);
    for _ in 0..DEPTH {
      html.push_str("<div>");
    }
    for _ in 0..DEPTH {
      html.push_str("</div>");
    }

    let result = std::panic::catch_unwind(|| parse_html(&html));
    assert!(
      result.is_ok(),
      "parse_html panicked on deeply nested markup"
    );
    assert!(
      result.unwrap().is_ok(),
      "parse_html returned error on deeply nested markup"
    );
  }

  #[test]
  fn convert_handle_to_node_can_return_none_without_panicking() {
    let dom = parse_document(RcDom::default(), ParseOpts::default()).one("<!--x-->".to_string());

    let mut stack = vec![dom.document.clone()];
    let mut comment = None;
    while let Some(handle) = stack.pop() {
      if matches!(&handle.data, NodeData::Comment { .. }) {
        comment = Some(handle);
        break;
      }
      for child in handle.children.borrow().iter() {
        stack.push(child.clone());
      }
    }

    let comment = comment.expect("expected html5ever to create a comment node");
    let mut deadline_counter = 0usize;
    let converted = convert_handle_to_node(&comment, QuirksMode::NoQuirks, &mut deadline_counter)
      .expect("convert handle");
    assert!(converted.is_none());
  }

  #[test]
  fn document_quirks_mode_defaults_to_no_quirks_with_doctype() {
    let dom = parse_html("<!doctype html><html><body></body></html>").expect("parse html");
    assert_eq!(
      dom.document_quirks_mode(),
      QuirksMode::NoQuirks,
      "HTML5 doctype should produce no-quirks mode"
    );
  }

  #[test]
  fn document_quirks_mode_enters_quirks_without_doctype() {
    let dom = parse_html("<html><body></body></html>").expect("parse html");
    assert_eq!(
      dom.document_quirks_mode(),
      QuirksMode::Quirks,
      "missing doctype should trigger quirks mode"
    );
  }

  #[test]
  fn parse_html_uses_empty_namespace_for_html_elements() {
    let dom =
      parse_html("<!doctype html><html><body><div id='x'></div></body></html>").expect("parse");
    let div = find_element_by_id(&dom, "x").expect("div element");
    assert_eq!(
      div.namespace(),
      Some(""),
      "HTML elements should store an empty namespace to avoid per-node allocations"
    );

    let div_ref = ElementRef::new(div);
    assert!(
      div_ref.has_namespace(HTML_NAMESPACE),
      "namespace matching should treat the empty namespace as HTML"
    );
  }

  #[test]
  fn declarative_shadow_dom_only_attaches_first_template() {
    let html = "<div id='host'><template shadowroot='open'><p id='first'>first</p></template><template shadowroot='closed'><p id='second'>second</p></template><p id='light'>light</p></div>";
    let dom = parse_html(html).expect("parse html");

    let host = find_element_by_id(&dom, "host").expect("host element");
    let shadow_roots: Vec<&DomNode> = host
      .children
      .iter()
      .filter(|child| matches!(child.node_type, DomNodeType::ShadowRoot { .. }))
      .collect();
    assert_eq!(
      shadow_roots.len(),
      1,
      "only the first declarative shadow template should attach"
    );

    assert!(
      shadow_roots[0]
        .children
        .iter()
        .any(|child| child.get_attribute_ref("id") == Some("first")),
      "shadow root should be populated from the first template's content"
    );

    let remaining_templates = host
      .children
      .iter()
      .filter(|child| {
        child
          .tag_name()
          .map(|name| name.eq_ignore_ascii_case("template"))
          .unwrap_or(false)
      })
      .count();
    assert_eq!(
      remaining_templates, 1,
      "subsequent shadow templates should remain inert in the light DOM"
    );
  }

  #[test]
  fn declarative_shadow_dom_records_delegates_focus() {
    let html = "<div id='host'><template shadowroot='open' shadowrootdelegatesfocus><slot></slot></template></div>";
    let dom = parse_html(html).expect("parse html");

    let host = find_element_by_id(&dom, "host").expect("host element");
    let shadow_root = host
      .children
      .iter()
      .find(|child| matches!(child.node_type, DomNodeType::ShadowRoot { .. }))
      .expect("shadow root attached");
    match shadow_root.node_type {
      DomNodeType::ShadowRoot {
        mode,
        delegates_focus,
      } => {
        assert_eq!(mode, ShadowRootMode::Open);
        assert!(
          delegates_focus,
          "shadowrootdelegatesfocus should be recorded on the shadow root"
        );
      }
      _ => panic!("expected shadow root child"),
    }
  }

  #[test]
  fn slot_in_svg_is_treated_as_element() {
    let dom = parse_html("<svg><slot id=\"s\"></slot></svg>").expect("parse html");

    let slot = find_element_by_id(&dom, "s").expect("slot element");
    match &slot.node_type {
      DomNodeType::Element {
        namespace,
        tag_name,
        ..
      } => {
        assert_eq!(namespace, SVG_NAMESPACE, "should retain SVG namespace");
        assert!(tag_name.eq_ignore_ascii_case("slot"));
      }
      other => panic!("expected element node, got {:?}", other),
    }
  }

  #[test]
  fn declarative_shadow_dom_skips_in_inert_template() {
    let html = "<template><div id='host'><template shadowroot='open'><p id='shadow'>shadow</p></template></div></template>";
    let dom = parse_html(html).expect("parse html");

    find_element_by_id(&dom, "host").expect("host element inside template content");
    assert!(
      !contains_shadow_root(&dom),
      "shadow roots should not attach inside inert template contents"
    );
  }

  #[test]
  fn declarative_shadow_dom_attaches_outside_inert_template() {
    let html =
      "<div id='host'><template shadowroot='open'><p id='shadow'>shadow</p></template></div>";
    let dom = parse_html(html).expect("parse html");

    let host = find_element_by_id(&dom, "host").expect("host element");
    let shadow_root = host
      .children
      .iter()
      .find(|child| matches!(child.node_type, DomNodeType::ShadowRoot { .. }))
      .expect("shadow root attached when not in an inert template");
    assert!(
      shadow_root
        .children
        .iter()
        .any(|child| child.get_attribute_ref("id") == Some("shadow")),
      "shadow root should include children from the declarative template"
    );
  }

  #[test]
  fn slot_assignment_ignores_template_contents() {
    let html = "<div id='host'><template shadowroot='open'><template><slot id='tmpl' name='a'></slot></template><slot id='real' name='a'></slot></template><span id='light' slot='a'></span></div>";
    let dom = parse_html(html).expect("parse html");
    let ids = enumerate_dom_ids(&dom);
    let assignment = compute_slot_assignment_with_ids(&dom, &ids).expect("slot assignment");

    let light = find_node_by_id(&dom, "light").expect("light node");
    let real_slot = find_node_by_id(&dom, "real").expect("real slot");
    let template_slot = find_node_by_id(&dom, "tmpl").expect("template slot");

    let light_id = ids
      .get(&(light as *const DomNode))
      .copied()
      .expect("light node id");
    let real_id = ids
      .get(&(real_slot as *const DomNode))
      .copied()
      .expect("real slot id");
    let template_id = ids
      .get(&(template_slot as *const DomNode))
      .copied()
      .expect("template slot id");

    let assigned = assignment
      .node_to_slot
      .get(&light_id)
      .expect("light assigned");
    assert_eq!(assigned.slot_node_id, real_id);
    assert_ne!(assigned.slot_node_id, template_id);
  }

  #[test]
  fn non_ascii_whitespace_slot_assignment_does_not_trim_nbsp_in_slot_attr() {
    let html = "<div id='host'><template shadowroot='open'><slot id='default'></slot><slot id='named' name='foo'></slot></template><span id='light' slot='&nbsp;foo'></span></div>";
    let dom = parse_html(html).expect("parse html");
    let ids = enumerate_dom_ids(&dom);
    let assignment = compute_slot_assignment_with_ids(&dom, &ids).expect("slot assignment");

    let light = find_node_by_id(&dom, "light").expect("light node");
    let default_slot = find_node_by_id(&dom, "default").expect("default slot");
    let named_slot = find_node_by_id(&dom, "named").expect("named slot");

    let light_id = ids
      .get(&(light as *const DomNode))
      .copied()
      .expect("light node id");
    let default_id = ids
      .get(&(default_slot as *const DomNode))
      .copied()
      .expect("default slot id");
    let named_id = ids
      .get(&(named_slot as *const DomNode))
      .copied()
      .expect("named slot id");

    let assigned = assignment
      .node_to_slot
      .get(&light_id)
      .expect("light assigned");
    assert_eq!(
      assigned.slot_node_id, default_id,
      "NBSP must not be treated as whitespace for slot name matching"
    );
    assert_ne!(assigned.slot_node_id, named_id);
  }

  #[test]
  fn part_export_map_ignores_template_contents() {
    let html = "<div id='host'><template shadowroot='open'><template><div id='tmpl' part='x'></div></template><div id='real' part='x'></div></template></div>";
    let dom = parse_html(html).expect("parse html");
    let ids = enumerate_dom_ids(&dom);
    let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

    let host = find_node_by_id(&dom, "host").expect("host");
    let real = find_node_by_id(&dom, "real").expect("real part element");
    let template = find_node_by_id(&dom, "tmpl").expect("template part element");

    let host_id = ids
      .get(&(host as *const DomNode))
      .copied()
      .expect("host id");
    let real_id = ids
      .get(&(real as *const DomNode))
      .copied()
      .expect("real id");
    let template_id = ids
      .get(&(template as *const DomNode))
      .copied()
      .expect("template id");

    let exports = map.exports_for_host(host_id).expect("host export map");
    let targets = exports.get("x").expect("part targets");
    assert!(targets.contains(&ExportedPartTarget::Element(real_id)));
    assert!(!targets.contains(&ExportedPartTarget::Element(template_id)));
  }

  #[test]
  fn has_relative_selector_does_not_match_inside_template_contents() {
    let dom = element(
      "div",
      vec![element("template", vec![element("span", vec![])])],
    );

    let mut input = ParserInput::new("span");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document();

    let mut ancestors = RelativeSelectorAncestorStack::new(&[]);
    let mut bloom_filter = BloomFilter::new();
    let mut deadline_counter = 0usize;

    assert!(
      !match_relative_selector(
        &selectors[0],
        &dom,
        &mut ancestors,
        &mut bloom_filter,
        false,
        &mut context,
        &mut deadline_counter,
      ),
      ":has should not traverse into inert template contents"
    );
  }

  fn matches(node: &DomNode, ancestors: &[&DomNode], pseudo: &PseudoClass) -> bool {
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
    let element_ref = ElementRef::with_ancestors(node, ancestors);
    element_ref.match_non_ts_pseudo_class(pseudo, &mut context)
  }

  fn parse_selector_list(selector_list: &str) -> SelectorList<FastRenderSelectorImpl> {
    let mut input = ParserInput::new(selector_list);
    let mut parser = Parser::new(&mut input);
    SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No)
      .expect("selector list should parse")
  }

  fn nth_of_cache_populations() -> u64 {
    super::NTH_OF_CACHE_POPULATIONS.with(|counter| counter.load(Ordering::Relaxed))
  }

  fn reset_nth_of_cache_populations() {
    super::NTH_OF_CACHE_POPULATIONS.with(|counter| counter.store(0, Ordering::Relaxed))
  }

  #[test]
  fn nth_child_of_selector_list_uses_nth_index_cache() {
    reset_nth_of_cache_populations();
    let foo_selectors = parse_selector_list(".foo");

    let mut children = Vec::new();
    for idx in 0..128usize {
      let class = if idx == 0 || idx == 64 || idx == 127 {
        vec![("class".to_string(), "foo".to_string())]
      } else {
        vec![]
      };
      children.push(DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: class,
        },
        children: vec![],
      });
    }
    let parent = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children,
    };

    let ancestors: Vec<&DomNode> = vec![&parent];
    let non_matching = &parent.children[1];
    let second_foo = &parent.children[64];
    let last_foo = &parent.children[127];

    let nth_child = PseudoClass::NthChild(0, 2, Some(foo_selectors.clone()));

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

    let non_matching_ref = ElementRef::with_ancestors(non_matching, &ancestors);
    assert!(
      !non_matching_ref.match_non_ts_pseudo_class(&nth_child, &mut context),
      "non-matching siblings should never match :nth-child(of ...)"
    );
    assert!(
      !non_matching_ref.match_non_ts_pseudo_class(&nth_child, &mut context),
      "non-matching siblings should not trigger repeated rescans"
    );

    let second_ref = ElementRef::with_ancestors(second_foo, &ancestors);
    assert!(
      second_ref.match_non_ts_pseudo_class(&nth_child, &mut context),
      "should match the 2nd .foo element"
    );

    assert_eq!(
      nth_of_cache_populations(),
      1,
      "nth-child(of ...) should populate nth-index cache once per parent+selector list"
    );

    let nth_last_child = PseudoClass::NthLastChild(0, 1, Some(foo_selectors));
    let last_ref = ElementRef::with_ancestors(last_foo, &ancestors);
    assert!(
      last_ref.match_non_ts_pseudo_class(&nth_last_child, &mut context),
      "should match the last .foo element via :nth-last-child(1 of .foo)"
    );
    assert!(
      !non_matching_ref.match_non_ts_pseudo_class(&nth_last_child, &mut context),
      "non-matching siblings should never match :nth-last-child(of ...)"
    );
    assert!(
      !non_matching_ref.match_non_ts_pseudo_class(&nth_last_child, &mut context),
      "non-matching siblings should not trigger repeated rescans for :nth-last-child(of ...)"
    );

    assert_eq!(
      nth_of_cache_populations(),
      2,
      "nth-last-child(of ...) should maintain an independent cached index map"
    );
  }

  #[test]
  fn enumerate_dom_ids_matches_legacy_preorder() {
    let dom = document(vec![
      element_with_attrs(
        "div",
        vec![("id", "host")],
        vec![
          DomNode {
            node_type: DomNodeType::ShadowRoot {
              mode: ShadowRootMode::Open,
              delegates_focus: false,
            },
            children: vec![element("span", vec![text("shadow")])],
          },
          element("p", vec![text("light")]),
        ],
      ),
      element("footer", vec![]),
    ]);

    let ids = enumerate_dom_ids(&dom);
    let legacy = enumerate_dom_ids_legacy(&dom);
    assert_eq!(ids, legacy);
  }

  #[test]
  fn dom_node_attribute_mutation_helpers_are_case_insensitive() {
    let mut node = element_with_attrs("div", vec![("Data-Fastr-Hidden", "false")], vec![]);

    node.set_attribute("data-fastr-hidden", "true");
    assert_eq!(node.get_attribute_ref("DATA-FASTR-HIDDEN"), Some("true"));
    assert_eq!(
      node
        .attributes_iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("data-fastr-hidden"))
        .count(),
      1
    );

    node.remove_attribute("DATA-FASTR-HIDDEN");
    assert_eq!(node.get_attribute_ref("data-fastr-hidden"), None);

    node.toggle_bool_attribute("disabled", true);
    assert_eq!(node.get_attribute_ref("DISABLED"), Some(""));
    node.toggle_bool_attribute("DISABLED", false);
    assert_eq!(node.get_attribute_ref("disabled"), None);

    let mut slot = DomNode {
      node_type: DomNodeType::Slot {
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("NaMe".to_string(), "x".to_string())],
        assigned: false,
      },
      children: vec![],
    };
    slot.set_attribute("name", "y");
    assert_eq!(slot.get_attribute_ref("NAME"), Some("y"));
    assert_eq!(
      slot
        .attributes_iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("name"))
        .count(),
      1
    );
    slot.remove_attribute("NAME");
    assert_eq!(slot.get_attribute_ref("name"), None);

    let mut text = text("hi");
    text.set_attribute("data-x", "1");
    text.remove_attribute("data-x");
    text.toggle_bool_attribute("data-x", true);
    assert_eq!(text.text_content(), Some("hi"));
    assert_eq!(text.get_attribute_ref("data-x"), None);
  }

  #[test]
  fn find_node_mut_by_preorder_id_matches_enumerate_dom_ids() {
    let mut dom = parse_html(
      r#"<!doctype html>
<div id="a">
  <template id="tpl"><span id="inside"></span></template>
  <p id="p"><b></b></p>
</div>"#,
    )
    .expect("parse_html");

    let ids = enumerate_dom_ids(&dom);
    let len = ids.len();
    let mut by_id: Vec<*const DomNode> = vec![std::ptr::null(); len + 1];
    for (&ptr, &id) in ids.iter() {
      by_id[id] = ptr;
    }

    assert!(find_node_mut_by_preorder_id(&mut dom, 0).is_none());
    assert!(find_node_mut_by_preorder_id(&mut dom, len + 1).is_none());

    for id in 1..=len {
      let node = find_node_mut_by_preorder_id(&mut dom, id)
        .unwrap_or_else(|| panic!("missing node for id {id}"));
      let ptr = node as *const DomNode;
      assert_eq!(
        ptr, by_id[id],
        "id {id} should resolve to the same node as enumerate_dom_ids"
      );
    }
  }

  #[test]
  fn deep_dom_traversals_do_not_overflow_stack() {
    set_selector_bloom_enabled(true);

    let depth = 100_000usize;
    let mut dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: vec![],
      },
      children: vec![],
    };

    for _ in 1..depth {
      dom = DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: String::new(),
          attributes: vec![],
        },
        children: vec![dom],
      };
    }

    let mut deadline_counter = 0usize;
    attach_shadow_roots(&mut dom, &mut deadline_counter).expect("attach_shadow_roots");
    apply_dom_compatibility_mutations(&mut dom, &mut deadline_counter)
      .expect("apply_dom_compatibility_mutations");

    let mut walked = 0usize;
    dom.walk_tree(&mut |_| walked += 1);
    assert_eq!(walked, depth);

    let ids = enumerate_dom_ids(&dom);
    assert_eq!(ids.len(), depth);

    let store = build_selector_bloom_store(&dom, &ids).expect("selector bloom store");
    let summary_len = match &store {
      SelectorBloomStore::Bits256(store) => store.summaries.len(),
      SelectorBloomStore::Bits512(store) => store.summaries.len(),
      SelectorBloomStore::Bits1024(store) => store.summaries.len(),
    };
    assert_eq!(summary_len, ids.len() + 1);
    assert!(store.summary_for_id(0).is_none());

    let div_hash = selector_bloom_hash("div");
    assert!(store
      .summary_for_id(1)
      .expect("root summary")
      .contains_hash(div_hash));
    assert!(store
      .summary_for_id(depth)
      .expect("leaf summary")
      .contains_hash(div_hash));

    drop(ids);
    drop(store);
    drop(dom);
  }

  #[test]
  fn selector_bloom_store_ids_align_when_templates_have_children() {
    set_selector_bloom_enabled(true);

    let dom = document(vec![element_with_attrs(
      "div",
      vec![("id", "root")],
      vec![
        element_with_attrs(
          "template",
          vec![("id", "tpl")],
          vec![element_with_attrs("span", vec![("id", "inert")], vec![])],
        ),
        element_with_attrs("p", vec![("id", "after")], vec![]),
      ],
    )]);

    let ids = enumerate_dom_ids(&dom);
    let store = build_selector_bloom_store(&dom, &ids).expect("selector bloom store");

    let summary_len = match &store {
      SelectorBloomStore::Bits256(store) => store.summaries.len(),
      SelectorBloomStore::Bits512(store) => store.summaries.len(),
      SelectorBloomStore::Bits1024(store) => store.summaries.len(),
    };
    assert_eq!(
      summary_len,
      ids.len() + 1,
      "selector bloom store should allocate one summary slot per node id, even when templates have children"
    );

    let after = find_node_by_id(&dom, "after").expect("after node");
    let after_id = ids
      .get(&(after as *const DomNode))
      .copied()
      .expect("after node id");
    assert!(
      store.summary_for_id(after_id).is_some(),
      "expected bloom summary for node after <template>"
    );

    let inert = find_node_by_id(&dom, "inert").expect("template content node");
    let inert_id = ids
      .get(&(inert as *const DomNode))
      .copied()
      .expect("template content node id");
    assert!(
      store.summary_for_id(inert_id).is_some(),
      "expected bloom summary for node inside <template> contents"
    );
  }

  #[test]
  fn deep_select_option_traversals_do_not_overflow_stack() {
    let depth = 100_000usize;

    let mut node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "option".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("selected".to_string(), String::new()),
          ("value".to_string(), "x".to_string()),
        ],
      },
      children: vec![],
    };

    for _ in 0..depth {
      node = DomNode {
        node_type: DomNodeType::Element {
          tag_name: "optgroup".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![node],
      };
    }

    let select = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![node],
    };

    let first_selected = first_selected_option(&select).expect("expected selected option");
    assert_eq!(first_selected.tag_name().unwrap_or(""), "option");
    assert_eq!(first_selected.get_attribute_ref("value"), Some("x"));

    let selected = single_select_selected_option(&select).expect("expected selected option");
    assert_eq!(selected.tag_name().unwrap_or(""), "option");
    assert_eq!(selected.get_attribute_ref("value"), Some("x"));
  }

  #[test]
  fn option_value_falls_back_to_normalized_option_text() {
    let option = element(
      "option",
      vec![
        text("  Foo \n"),
        element("span", vec![text("Bar")]),
        text("\tBaz  "),
      ],
    );
    assert_eq!(option_value_from_node(&option), "Foo Bar Baz");
  }

  #[test]
  fn option_text_ignores_script_descendants() {
    let option = element(
      "option",
      vec![text("Foo "), element("script", vec![text("BAR")]), text(" Baz")],
    );
    assert_eq!(option_value_from_node(&option), "Foo Baz");
  }

  #[test]
  fn select_value_multiple_returns_first_selected_option_value() {
    let select = element_with_attrs(
      "select",
      vec![("multiple", "")],
      vec![
        element_with_attrs("option", vec![("selected", "")], vec![text("One")]),
        element_with_attrs("option", vec![("selected", "")], vec![text("Two")]),
      ],
    );

    let value = ElementRef::new(&select).control_value();
    assert_eq!(value, Some("One".to_string()));
  }

  #[test]
  fn select_value_multiple_returns_empty_string_when_no_options_selected() {
    let select = element_with_attrs(
      "select",
      vec![("multiple", "")],
      vec![element("option", vec![text("One")]), element("option", vec![text("Two")])],
    );

    let value = ElementRef::new(&select).control_value();
    assert_eq!(value, Some(String::new()));
  }

  #[test]
  fn required_multi_select_is_invalid_when_no_options_selected() {
    let select = element_with_attrs(
      "select",
      vec![("multiple", ""), ("required", "")],
      vec![element("option", vec![text("One")]), element("option", vec![text("Two")])],
    );

    assert!(!ElementRef::new(&select).is_valid_control());
  }

  #[test]
  fn required_multi_select_is_invalid_when_only_disabled_options_selected() {
    let select = element_with_attrs(
      "select",
      vec![("multiple", ""), ("required", "")],
      vec![
        element_with_attrs("option", vec![("selected", ""), ("disabled", "")], vec![text("One")]),
        element("option", vec![text("Two")]),
      ],
    );

    assert!(!ElementRef::new(&select).is_valid_control());
  }

  #[test]
  fn required_multi_select_is_valid_when_an_enabled_option_is_selected() {
    let select = element_with_attrs(
      "select",
      vec![("multiple", ""), ("required", "")],
      vec![
        element_with_attrs("option", vec![("selected", ""), ("disabled", "")], vec![text("One")]),
        element_with_attrs("option", vec![("selected", "")], vec![text("Two")]),
      ],
    );

    assert!(ElementRef::new(&select).is_valid_control());
  }

  #[test]
  fn number_input_is_invalid_for_non_finite_value() {
    let nan_input = element_with_attrs("input", vec![("type", "number"), ("value", "NaN")], vec![]);
    assert!(!ElementRef::new(&nan_input).is_valid_control());

    let inf_input = element_with_attrs(
      "input",
      vec![("type", "number"), ("value", "Infinity")],
      vec![],
    );
    assert!(!ElementRef::new(&inf_input).is_valid_control());
  }

  #[test]
  fn text_input_pattern_mismatch_sets_validity_flag() {
    let input = element_with_attrs(
      "input",
      vec![("pattern", "[0-9]+"), ("value", "abc")],
      vec![],
    );
    let state = forms_validation::validity_state(&ElementRef::new(&input)).expect("validity state");
    assert!(state.pattern_mismatch);
    assert!(!state.valid);
  }

  #[test]
  fn minlength_and_maxlength_set_too_short_and_too_long_flags() {
    let too_short = element_with_attrs("input", vec![("minlength", "5"), ("value", "abc")], vec![]);
    let state =
      forms_validation::validity_state(&ElementRef::new(&too_short)).expect("validity state");
    assert!(state.too_short);
    assert!(!state.valid);

    let too_long = element_with_attrs("input", vec![("maxlength", "2"), ("value", "abc")], vec![]);
    let state =
      forms_validation::validity_state(&ElementRef::new(&too_long)).expect("validity state");
    assert!(state.too_long);
    assert!(!state.valid);

    let textarea = element_with_attrs("textarea", vec![("minlength", "3")], vec![text("hi")]);
    let state =
      forms_validation::validity_state(&ElementRef::new(&textarea)).expect("validity state");
    assert!(state.too_short);
    assert!(!state.valid);
  }

  #[test]
  fn number_step_mismatch_sets_step_mismatch_flag() {
    let input = element_with_attrs(
      "input",
      vec![("type", "number"), ("step", "2"), ("value", "3")],
      vec![],
    );
    let state = forms_validation::validity_state(&ElementRef::new(&input)).expect("validity state");
    assert!(state.step_mismatch);
    assert!(!state.valid);
  }

  #[test]
  fn email_and_url_type_mismatch_set_type_mismatch_flag() {
    let email = element_with_attrs(
      "input",
      vec![("type", "email"), ("value", "not-an-email")],
      vec![],
    );
    let state = forms_validation::validity_state(&ElementRef::new(&email)).expect("validity state");
    assert!(state.type_mismatch);
    assert!(!state.valid);

    let url = element_with_attrs(
      "input",
      vec![("type", "url"), ("value", "example.com")],
      vec![],
    );
    let state = forms_validation::validity_state(&ElementRef::new(&url)).expect("validity state");
    assert!(state.type_mismatch);
    assert!(!state.valid);
  }

  #[test]
  fn number_value_attribute_invalid_sanitizes_to_empty_and_does_not_set_bad_input() {
    let number = element_with_attrs("input", vec![("type", "number"), ("value", "abc")], vec![]);
    assert_eq!(input_number_value_string(&number), Some(String::new()));
    assert_eq!(ElementRef::new(&number).control_value(), Some(String::new()));

    let state =
      forms_validation::validity_state(&ElementRef::new(&number)).expect("validity state");
    assert!(!state.bad_input);
    assert!(state.valid);
    assert!(!state.value_missing);

    let whitespace =
      element_with_attrs("input", vec![("type", "number"), ("value", "  5\t")], vec![]);
    assert_eq!(
      input_number_value_string(&whitespace).as_deref(),
      Some("5"),
      "number inputs should trim ASCII whitespace"
    );

    let required =
      element_with_attrs("input", vec![("type", "number"), ("value", "abc"), ("required", "")], vec![]);
    let state =
      forms_validation::validity_state(&ElementRef::new(&required)).expect("validity state");
    assert!(!state.bad_input);
    assert!(!state.valid);
    assert!(state.value_missing);
  }

  #[test]
  fn date_time_value_attribute_invalid_sanitizes_to_empty_and_does_not_set_bad_input() {
    let date = element_with_attrs("input", vec![("type", "date"), ("value", "2020-13-01")], vec![]);
    assert_eq!(input_date_value_string(&date), Some(String::new()));
    assert_eq!(ElementRef::new(&date).control_value(), Some(String::new()));
    let state = forms_validation::validity_state(&ElementRef::new(&date)).expect("validity state");
    assert!(!state.bad_input);
    assert!(state.valid);
    assert!(!state.value_missing);

    let time = element_with_attrs("input", vec![("type", "time"), ("value", "25:00")], vec![]);
    assert_eq!(input_time_value_string(&time), Some(String::new()));
    assert_eq!(ElementRef::new(&time).control_value(), Some(String::new()));
    let state = forms_validation::validity_state(&ElementRef::new(&time)).expect("validity state");
    assert!(!state.bad_input);
    assert!(state.valid);
    assert!(!state.value_missing);

    let datetime_local = element_with_attrs(
      "input",
      vec![("type", "datetime-local"), ("value", "2020-01-01T25:00")],
      vec![],
    );
    assert_eq!(input_datetime_local_value_string(&datetime_local), Some(String::new()));
    assert_eq!(
      ElementRef::new(&datetime_local).control_value(),
      Some(String::new())
    );
    let state =
      forms_validation::validity_state(&ElementRef::new(&datetime_local)).expect("validity state");
    assert!(!state.bad_input);
    assert!(state.valid);
    assert!(!state.value_missing);

    let month = element_with_attrs("input", vec![("type", "month"), ("value", "2020-13")], vec![]);
    assert_eq!(input_month_value_string(&month), Some(String::new()));
    assert_eq!(ElementRef::new(&month).control_value(), Some(String::new()));
    let state = forms_validation::validity_state(&ElementRef::new(&month)).expect("validity state");
    assert!(!state.bad_input);
    assert!(state.valid);
    assert!(!state.value_missing);

    let week = element_with_attrs("input", vec![("type", "week"), ("value", "2020-W99")], vec![]);
    assert_eq!(input_week_value_string(&week), Some(String::new()));
    assert_eq!(ElementRef::new(&week).control_value(), Some(String::new()));
    let state = forms_validation::validity_state(&ElementRef::new(&week)).expect("validity state");
    assert!(!state.bad_input);
    assert!(state.valid);
    assert!(!state.value_missing);

    let required_date = element_with_attrs(
      "input",
      vec![("type", "date"), ("value", "2020-13-01"), ("required", "")],
      vec![],
    );
    let state =
      forms_validation::validity_state(&ElementRef::new(&required_date)).expect("validity state");
    assert!(!state.bad_input);
    assert!(!state.valid);
    assert!(state.value_missing);
  }

  #[test]
  fn file_required_sets_value_missing_flag() {
    let input = element_with_attrs("input", vec![("type", "file"), ("required", "")], vec![]);
    let state = forms_validation::validity_state(&ElementRef::new(&input)).expect("validity state");
    assert!(state.value_missing);
    assert!(!state.valid);
  }

  #[test]
  fn color_input_is_always_valid_even_when_required() {
    let input = element_with_attrs("input", vec![("type", "color"), ("required", "")], vec![]);
    let state = forms_validation::validity_state(&ElementRef::new(&input)).expect("validity state");
    assert!(state.valid);
    assert!(!state.value_missing);
    assert!(ElementRef::new(&input).is_valid_control());
  }

  #[test]
  fn select_value_multiple_includes_disabled_selected_placeholder() {
    let select = element_with_attrs(
      "select",
      vec![("multiple", "")],
      vec![
        element_with_attrs(
          "option",
          vec![("selected", ""), ("disabled", ""), ("value", "")],
          vec![text("Choose...")],
        ),
        element_with_attrs("option", vec![("selected", ""), ("value", "two")], vec![text("Two")]),
      ],
    );

    let value = ElementRef::new(&select).control_value();
    assert_eq!(value, Some(String::new()));
  }

  #[test]
  fn select_value_single_uses_last_selected_option_in_tree_order() {
    let select = element_with_attrs(
      "select",
      vec![],
      vec![
        element_with_attrs("option", vec![("selected", "")], vec![text("One")]),
        element_with_attrs("option", vec![("selected", "")], vec![text("Two")]),
      ],
    );

    assert_eq!(ElementRef::new(&select).control_value(), Some("Two".to_string()));

    let ancestors: [&DomNode; 1] = [&select];
    let option_one_ref = ElementRef::with_ancestors(&select.children[0], &ancestors);
    let option_two_ref = ElementRef::with_ancestors(&select.children[1], &ancestors);
    assert!(!option_one_ref.is_checked());
    assert!(option_two_ref.is_checked());
  }

  #[test]
  fn selector_bloom_store_matches_legacy_map() {
    set_selector_bloom_enabled(true);

    let dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("id".to_string(), "host".to_string())],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "span".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("class".to_string(), "light".to_string())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::ShadowRoot {
            mode: ShadowRootMode::Open,
            delegates_focus: false,
          },
          children: vec![DomNode {
            node_type: DomNodeType::Element {
              tag_name: "span".to_string(),
              namespace: HTML_NAMESPACE.to_string(),
              attributes: vec![("class".to_string(), "shadow".to_string())],
            },
            children: vec![],
          }],
        },
      ],
    };

    let id_map = enumerate_dom_ids(&dom);
    let store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");
    fn assert_store_matches_legacy<const WORDS: usize>(
      dom: &DomNode,
      id_map: &HashMap<*const DomNode, usize>,
      store: &SelectorBloomStore,
    ) {
      let legacy =
        build_selector_bloom_map_legacy::<WORDS>(dom).expect("legacy selector bloom map");
      for (ptr, id) in id_map.iter() {
        // Safety: ids are built from stable DOM pointers.
        let node = unsafe { &**ptr };
        if !node.is_element() {
          continue;
        }
        let summary_store = store
          .summary_for_id(*id)
          .unwrap_or_else(|| panic!("store missing summary for node_id={id}"));
        let summary_legacy = legacy
          .get(ptr)
          .unwrap_or_else(|| panic!("legacy missing summary for node_id={id}"));
        assert_eq!(
          summary_store.words(),
          summary_legacy.as_ref(),
          "selector bloom summary mismatch for node_id={id}"
        );
      }
    }

    assert!(
      store.summary_for_id(0).is_none(),
      "selector bloom store index 0 must remain unused"
    );
    match selector_bloom_summary_bits() {
      256 => assert_store_matches_legacy::<4>(&dom, &id_map, &store),
      512 => assert_store_matches_legacy::<8>(&dom, &id_map, &store),
      1024 => assert_store_matches_legacy::<16>(&dom, &id_map, &store),
      bits => panic!("unexpected selector bloom summary bits: {bits}"),
    }
  }

  #[test]
  fn bloom_pruning_skips_expensive_evaluations() {
    reset_has_counters();
    set_selector_bloom_enabled(true);
    let dom = element("div", vec![element("span", vec![])]);
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document().with_selector_blooms(Some(&bloom_store));

    let mut input = ParserInput::new(".missing");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    let anchor = ElementRef::with_ancestors(&dom, &[]).with_node_id(1);

    assert!(
      !matches_has_relative(&anchor, &selectors, &mut context),
      ":has should prune when bloom hash is absent"
    );

    let counters = capture_has_counters();
    assert_eq!(counters.prunes, 1);
    assert_eq!(counters.evaluated, 0);
  }

  #[test]
  fn bloom_pruning_preserves_matches() {
    reset_has_counters();
    set_selector_bloom_enabled(true);
    let dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".into(),
        namespace: HTML_NAMESPACE.into(),
        attributes: vec![],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".into(),
          namespace: HTML_NAMESPACE.into(),
          attributes: vec![("class".into(), "foo".into())],
        },
        children: vec![],
      }],
    };
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document().with_selector_blooms(Some(&bloom_store));

    let mut input = ParserInput::new(".foo");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    let anchor = ElementRef::with_ancestors(&dom, &[]).with_node_id(1);

    assert!(
      matches_has_relative(&anchor, &selectors, &mut context),
      ":has should still evaluate when hashes are present"
    );

    let counters = capture_has_counters();
    assert_eq!(counters.prunes, 0);
    assert_eq!(counters.evaluated, 1);
  }

  #[test]
  fn nested_has_is_invalid() {
    // Selectors Level 4 disallows nested `:has()`.
    let mut input = ParserInput::new(".a:has(.b:has(.c))");
    let mut parser = Parser::new(&mut input);
    assert!(
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).is_err(),
      "nested :has() should be rejected"
    );
  }

  #[test]
  fn nested_has_is_forgiven_inside_is() {
    // Nested `:has()` is invalid, but selector lists in `:is()` are forgiving, so the invalid
    // selector should be dropped and the remaining selector list should still parse and match.
    let selector = parse_selector(".a:has(:is(.b, :has(.c)))");

    let dom = element_with_attrs(
      "div",
      vec![("class", "a")],
      vec![element_with_attrs("div", vec![("class", "b")], vec![])],
    );
    let anchor = ElementRef::new(&dom);
    assert!(
      selector_matches(&anchor, &selector),
      "expected nested :has() inside :is() to be ignored, leaving :is(.b)"
    );
  }

  #[test]
  fn bloom_summary_pruning_handles_is_breakouts() {
    // `:is()` can contain selectors that match ancestors of the current element, which may live
    // outside the :has() anchor's subtree. Bloom-summary pruning must never treat those ancestor
    // selectors as mandatory within the anchor subtree (no false negatives).
    reset_has_counters();
    set_selector_bloom_enabled(true);

    let dom = element_with_attrs(
      "div",
      vec![("class", "a")],
      vec![element_with_attrs(
        "div",
        vec![("class", "anchor")],
        vec![element_with_attrs(
          "div",
          vec![("class", "b")],
          vec![element_with_attrs("div", vec![("class", "c")], vec![])],
        )],
      )],
    );
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let selector = parse_selector(".anchor:has(:is(.a .b) .c)");
    let anchor_node = dom.children.first().expect("anchor exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor id exists");
    let anchor_ancestors = [&dom];
    let anchor = ElementRef::with_ancestors(anchor_node, &anchor_ancestors).with_node_id(anchor_id);

    // First verify the selector semantics without bloom-summary pruning.
    let matched_without_summary = {
      let mut caches = SelectorCaches::default();
      caches.set_epoch(next_selector_cache_epoch());
      let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        QuirksMode::NoQuirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
      );
      context.extra_data = ShadowMatchData::for_document();
      matches_selector(&selector, 0, None, &anchor, &mut context)
    };
    assert!(
      matched_without_summary,
      "expected selector semantics to match when bloom-summary pruning is disabled"
    );

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document()
      .with_selector_blooms(Some(&bloom_store))
      .with_node_to_id(Some(&id_map));

    assert!(
      matches_selector(&selector, 0, None, &anchor, &mut context),
      "expected :has() selector to match even though `.a` is outside the anchor subtree"
    );
  }

  #[test]
  fn bloom_summary_pruning_handles_terminal_is_breakouts() {
    // Like `bloom_summary_pruning_handles_is_breakouts`, but where the `:is()` pseudo-class is the
    // rightmost compound in the relative selector. Bloom-summary pruning must still avoid treating
    // `.a` as mandatory within the anchor subtree.
    reset_has_counters();
    set_selector_bloom_enabled(true);

    let dom = element_with_attrs(
      "div",
      vec![("class", "a")],
      vec![element_with_attrs(
        "div",
        vec![("class", "anchor")],
        vec![element_with_attrs("div", vec![("class", "b")], vec![])],
      )],
    );
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let selector = parse_selector(".anchor:has(:is(.a .b))");
    let anchor_node = dom.children.first().expect("anchor exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor id exists");
    let anchor_ancestors = [&dom];
    let anchor = ElementRef::with_ancestors(anchor_node, &anchor_ancestors).with_node_id(anchor_id);

    let matched_without_summary = {
      let mut caches = SelectorCaches::default();
      caches.set_epoch(next_selector_cache_epoch());
      let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        QuirksMode::NoQuirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
      );
      context.extra_data = ShadowMatchData::for_document();
      matches_selector(&selector, 0, None, &anchor, &mut context)
    };
    assert!(
      matched_without_summary,
      "expected selector semantics to match when bloom-summary pruning is disabled"
    );

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document()
      .with_selector_blooms(Some(&bloom_store))
      .with_node_to_id(Some(&id_map));

    assert!(
      matches_selector(&selector, 0, None, &anchor, &mut context),
      "expected :has() selector to match even though `.a` is outside the anchor subtree"
    );
  }

  #[test]
  fn bloom_summary_pruning_handles_is_breakouts_in_next_sibling() {
    // Same breakout scenario, but when the relative selector stays on the next sibling itself.
    reset_has_counters();
    set_selector_bloom_enabled(true);

    let dom = element_with_attrs(
      "div",
      vec![("class", "a")],
      vec![
        element_with_attrs("div", vec![("class", "anchor")], vec![]),
        element_with_attrs("div", vec![("class", "b")], vec![]),
      ],
    );

    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let selector = parse_selector(".anchor:has(+ :is(.a .b))");
    let anchor_node = dom.children.first().expect("anchor exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor id exists");
    let anchor_ancestors = [&dom];
    let anchor = ElementRef::with_ancestors(anchor_node, &anchor_ancestors).with_node_id(anchor_id);

    let matched_without_summary = {
      let mut caches = SelectorCaches::default();
      caches.set_epoch(next_selector_cache_epoch());
      let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        QuirksMode::NoQuirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
      );
      context.extra_data = ShadowMatchData::for_document();
      matches_selector(&selector, 0, None, &anchor, &mut context)
    };
    assert!(
      matched_without_summary,
      "expected selector semantics to match when bloom-summary pruning is disabled"
    );

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document()
      .with_selector_blooms(Some(&bloom_store))
      .with_node_to_id(Some(&id_map));

    assert!(
      matches_selector(&selector, 0, None, &anchor, &mut context),
      "expected :has(+ ...) selector to match even though `.a` is outside the sibling subtree"
    );
  }

  #[test]
  fn bloom_summary_pruning_handles_is_breakouts_in_next_sibling_subtree() {
    // Breakouts also apply when the relative selector traverses into a sibling subtree: inner
    // selectors can require ancestors that live outside the sibling subtree (for example, on the
    // common parent of the anchor + sibling).
    reset_has_counters();
    set_selector_bloom_enabled(true);

    let dom = element_with_attrs(
      "div",
      vec![("class", "a")],
      vec![
        element_with_attrs("div", vec![("class", "anchor")], vec![]),
        element_with_attrs(
          "div",
          vec![("class", "b")],
          vec![element_with_attrs("div", vec![("class", "c")], vec![])],
        ),
      ],
    );

    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let selector = parse_selector(".anchor:has(+ :is(.a .b) .c)");
    let anchor_node = dom.children.first().expect("anchor exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor id exists");
    let anchor_ancestors = [&dom];
    let anchor = ElementRef::with_ancestors(anchor_node, &anchor_ancestors).with_node_id(anchor_id);

    // Verify semantics without bloom-summary pruning (but with other pruning/caching enabled).
    let matched_without_summary = {
      let mut caches = SelectorCaches::default();
      caches.set_epoch(next_selector_cache_epoch());
      let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        QuirksMode::NoQuirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
      );
      context.extra_data = ShadowMatchData::for_document();
      matches_selector(&selector, 0, None, &anchor, &mut context)
    };
    assert!(
      matched_without_summary,
      "expected selector semantics to match when bloom-summary pruning is disabled"
    );

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document()
      .with_selector_blooms(Some(&bloom_store))
      .with_node_to_id(Some(&id_map));

    assert!(
      matches_selector(&selector, 0, None, &anchor, &mut context),
      "expected :has(+ ...) selector to match even though `.a` is outside the sibling subtree"
    );
  }

  #[test]
  fn bloom_pruning_next_sibling_prunes_when_absent() {
    reset_has_counters();
    set_selector_bloom_enabled(true);

    let dom = element("div", vec![element("span", vec![])]);
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document()
      .with_selector_blooms(Some(&bloom_store))
      .with_node_to_id(Some(&id_map));

    let mut input = ParserInput::new("+ .missing");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);

    let anchor_node = dom.children.first().expect("anchor exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor id exists");
    let anchor_ancestors = [&dom];
    let anchor = ElementRef::with_ancestors(anchor_node, &anchor_ancestors).with_node_id(anchor_id);

    assert!(
      !matches_has_relative(&anchor, &selectors, &mut context),
      ":has(+ ...) should not match without a next sibling"
    );

    let counters = capture_has_counters();
    assert_eq!(counters.prunes, 1);
    assert_eq!(counters.evaluated, 0);
  }

  #[test]
  fn bloom_pruning_next_sibling_prunes_when_hash_missing() {
    reset_has_counters();
    set_selector_bloom_enabled(true);

    let dom = element(
      "div",
      vec![
        element("span", vec![]),
        element_with_attrs("span", vec![("class", "present")], vec![]),
      ],
    );
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document()
      .with_selector_blooms(Some(&bloom_store))
      .with_node_to_id(Some(&id_map));

    let mut input = ParserInput::new("+ .missing");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);

    let anchor_node = dom.children.first().expect("anchor exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor id exists");
    let anchor_ancestors = [&dom];
    let anchor = ElementRef::with_ancestors(anchor_node, &anchor_ancestors).with_node_id(anchor_id);

    assert!(
      !matches_has_relative(&anchor, &selectors, &mut context),
      ":has(+ .missing) should prune when the next sibling subtree lacks the hash"
    );

    let counters = capture_has_counters();
    assert_eq!(counters.prunes, 1);
    assert_eq!(counters.filter_prunes, 0);
    assert_eq!(counters.evaluated, 0);
  }

  #[test]
  fn bloom_pruning_next_sibling_preserves_matches() {
    reset_has_counters();
    set_selector_bloom_enabled(true);

    let dom = element(
      "div",
      vec![
        element("span", vec![]),
        element_with_attrs("span", vec![("class", "hit")], vec![]),
      ],
    );
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document()
      .with_selector_blooms(Some(&bloom_store))
      .with_node_to_id(Some(&id_map));

    let mut input = ParserInput::new("+ .hit");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);

    let anchor_node = dom.children.first().expect("anchor exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor id exists");
    let anchor_ancestors = [&dom];
    let anchor = ElementRef::with_ancestors(anchor_node, &anchor_ancestors).with_node_id(anchor_id);

    assert!(
      matches_has_relative(&anchor, &selectors, &mut context),
      ":has(+ .hit) should still match the next sibling"
    );

    let counters = capture_has_counters();
    assert_eq!(counters.prunes, 0);
    assert_eq!(counters.evaluated, 1);
  }

  #[test]
  fn bloom_pruning_ignores_inert_template_contents() {
    reset_has_counters();
    set_selector_bloom_enabled(true);
    let dom = element(
      "div",
      vec![element(
        "template",
        vec![element_with_attrs("span", vec![("class", "hit")], vec![])],
      )],
    );
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let host_summary = bloom_store.summary_for_id(1).expect("host summary");
    let template_summary = bloom_store.summary_for_id(2).expect("template summary");
    let hit_summary = bloom_store.summary_for_id(3).expect("hit summary");

    let hit_hash = selector_bloom_hash("hit");
    assert!(
      hit_summary.contains_hash(hit_hash),
      "template descendant should include its own selector bloom hashes"
    );
    assert!(
      !template_summary.contains_hash(hit_hash),
      "template summary should not merge template contents"
    );
    assert!(
      !host_summary.contains_hash(hit_hash),
      "ancestor summaries should not see inert template contents"
    );

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document().with_selector_blooms(Some(&bloom_store));

    let mut input = ParserInput::new(".hit");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    let anchor = ElementRef::with_ancestors(&dom, &[]).with_node_id(1);

    assert!(
      !matches_has_relative(&anchor, &selectors, &mut context),
      ":has should not match inert template contents"
    );

    let counters = capture_has_counters();
    assert_eq!(counters.summary_prunes(), 1);
    assert_eq!(counters.filter_prunes, 0);
    assert_eq!(counters.evaluated, 0);
  }

  #[test]
  fn bloom_pruning_skips_expensive_evaluations_in_quirks_mode() {
    reset_has_counters();
    set_selector_bloom_enabled(true);
    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::Quirks,
      },
      children: vec![element("div", vec![element("span", vec![])])],
    };
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let anchor_node = dom.children.first().expect("anchor node exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor node id");

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::Quirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document().with_selector_blooms(Some(&bloom_store));

    let mut input = ParserInput::new(".missing");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    let anchor = ElementRef::with_ancestors(anchor_node, &[]).with_node_id(anchor_id);

    assert!(
      !matches_has_relative(&anchor, &selectors, &mut context),
      ":has should prune when bloom hash is absent (quirks mode)"
    );

    let counters = capture_has_counters();
    assert_eq!(counters.prunes, 1);
    assert_eq!(counters.evaluated, 0);
  }

  #[test]
  fn bloom_pruning_preserves_matches_in_quirks_mode() {
    reset_has_counters();
    set_selector_bloom_enabled(true);
    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::Quirks,
      },
      children: vec![element(
        "div",
        vec![element_with_attrs("span", vec![("class", "FOO")], vec![])],
      )],
    };
    let id_map = enumerate_dom_ids(&dom);
    let bloom_store = build_selector_bloom_store(&dom, &id_map).expect("selector bloom store");

    let anchor_node = dom.children.first().expect("anchor node exists");
    let anchor_id = *id_map
      .get(&(anchor_node as *const DomNode))
      .expect("anchor node id");

    let mut caches = SelectorCaches::default();
    caches.set_epoch(next_selector_cache_epoch());
    let mut context = MatchingContext::new(
      MatchingMode::Normal,
      None,
      &mut caches,
      QuirksMode::Quirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document().with_selector_blooms(Some(&bloom_store));

    let mut input = ParserInput::new(".foo");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    let anchor = ElementRef::with_ancestors(anchor_node, &[]).with_node_id(anchor_id);

    assert!(
      matches_has_relative(&anchor, &selectors, &mut context),
      ":has should still match in quirks mode with case-insensitive class selectors"
    );

    let counters = capture_has_counters();
    assert_eq!(counters.prunes, 0);
    assert_eq!(counters.evaluated, 1);
  }

  fn eval_relative_selector_with_ancestor_bloom(
    dom: &DomNode,
    selector: &RelativeSelector<FastRenderSelectorImpl>,
    use_ancestor_bloom: bool,
  ) -> bool {
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

    let anchor = ElementRef::with_ancestors(dom, &[]);
    context.nest_for_relative_selector(anchor.opaque(), |ctx| {
      ctx.nest_for_scope(Some(anchor.opaque()), |ctx| {
        let mut ancestors = RelativeSelectorAncestorStack::new(anchor.all_ancestors);
        let mut deadline_counter = 0usize;
        let mut ancestor_bloom_filter = BloomFilter::new();

        let matched = match_relative_selector(
          selector,
          anchor.node,
          &mut ancestors,
          &mut ancestor_bloom_filter,
          use_ancestor_bloom,
          ctx,
          &mut deadline_counter,
        );
        debug_assert_eq!(ancestors.len(), ancestors.baseline_len());
        debug_assert!(!use_ancestor_bloom || ancestor_bloom_filter.is_zeroed());
        matched
      })
    })
  }

  #[test]
  fn has_relative_selector_ancestor_bloom_matches_without_bloom() {
    let dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".into(),
        namespace: HTML_NAMESPACE.into(),
        attributes: vec![],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".into(),
          namespace: HTML_NAMESPACE.into(),
          attributes: vec![("class".into(), "a".into())],
        },
        children: vec![DomNode {
          node_type: DomNodeType::Element {
            tag_name: "div".into(),
            namespace: HTML_NAMESPACE.into(),
            attributes: vec![("class".into(), "b".into())],
          },
          children: vec![DomNode {
            node_type: DomNodeType::Element {
              tag_name: "div".into(),
              namespace: HTML_NAMESPACE.into(),
              attributes: vec![("class".into(), "c".into())],
            },
            children: vec![],
          }],
        }],
      }],
    };

    let mut input = ParserInput::new(".a .b .c");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    assert_eq!(selectors.len(), 1);

    let with_bloom = eval_relative_selector_with_ancestor_bloom(&dom, &selectors[0], true);
    let without_bloom = eval_relative_selector_with_ancestor_bloom(&dom, &selectors[0], false);
    assert!(with_bloom);
    assert_eq!(with_bloom, without_bloom);
  }

  #[test]
  fn deep_has_relative_selector_subtree_does_not_overflow_stack() {
    let depth = 100_000usize;

    let mut dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".into(),
        namespace: HTML_NAMESPACE.into(),
        attributes: vec![("class".into(), "target".into())],
      },
      children: vec![],
    };

    for _ in 0..depth {
      dom = DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".into(),
          namespace: HTML_NAMESPACE.into(),
          attributes: vec![],
        },
        children: vec![dom],
      };
    }

    let mut input = ParserInput::new(".target");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    assert_eq!(selectors.len(), 1);

    let with_bloom = eval_relative_selector_with_ancestor_bloom(&dom, &selectors[0], true);
    let without_bloom = eval_relative_selector_with_ancestor_bloom(&dom, &selectors[0], false);
    assert!(with_bloom);
    assert_eq!(with_bloom, without_bloom);
  }

  #[test]
  fn deep_subtree_has_content_does_not_overflow_stack() {
    // `:empty` uses `subtree_has_content` to treat shadow roots/documents as transparent wrappers
    // while still short-circuiting for normal element/text nodes. Keep this non-recursive so
    // pathological DOM shapes cannot overflow the call stack.
    let depth = 20_000usize;
    let mut node = DomNode {
      node_type: DomNodeType::Text {
        content: "x".to_string(),
      },
      children: vec![],
    };
    for _ in 0..depth {
      node = DomNode {
        node_type: DomNodeType::ShadowRoot {
          mode: ShadowRootMode::Open,
          delegates_focus: false,
        },
        children: vec![node],
      };
    }

    assert!(ElementRef::subtree_has_content(&node));
  }

  #[test]
  fn has_relative_selector_ancestor_bloom_rejects_by_ancestor_chain() {
    // Contains `.a`, `.b` and many `.c` nodes, but `.b` is not a descendant of `.a`,
    // so `.a .b .c` can never match.
    let dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".into(),
        namespace: HTML_NAMESPACE.into(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "div".into(),
            namespace: HTML_NAMESPACE.into(),
            attributes: vec![("class".into(), "a".into())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "div".into(),
            namespace: HTML_NAMESPACE.into(),
            attributes: vec![("class".into(), "b".into())],
          },
          children: (0..32)
            .map(|_| DomNode {
              node_type: DomNodeType::Element {
                tag_name: "div".into(),
                namespace: HTML_NAMESPACE.into(),
                attributes: vec![("class".into(), "c".into())],
              },
              children: vec![],
            })
            .collect(),
        },
      ],
    };

    let mut input = ParserInput::new(".a .b .c");
    let mut parser = Parser::new(&mut input);
    let list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::ForHas).expect("parse");
    let selectors = build_relative_selectors(list);
    assert_eq!(selectors.len(), 1);

    let with_bloom = eval_relative_selector_with_ancestor_bloom(&dom, &selectors[0], true);
    let without_bloom = eval_relative_selector_with_ancestor_bloom(&dom, &selectors[0], false);
    assert!(!with_bloom);
    assert_eq!(with_bloom, without_bloom);
  }

  #[test]
  fn collect_text_codepoints_skips_template_contents() {
    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![
        element("div", vec![text("abc")]),
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "template".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![text("Ωש")],
        },
      ],
    };

    let codepoints = collect_text_codepoints(&dom).unwrap();
    let expected: Vec<u32> = vec!['a', 'b', 'c'].into_iter().map(|c| c as u32).collect();
    assert_eq!(codepoints, expected);
  }

  #[test]
  fn collect_text_codepoints_includes_inert_contents() {
    let dom = document(vec![
      element("div", vec![text("a")]),
      element_with_attrs("div", vec![("inert", "")], vec![text("b")]),
      element_with_attrs("div", vec![("data-fastr-inert", "true")], vec![text("c")]),
      element_with_attrs("div", vec![("hidden", "")], vec![text("d")]),
    ]);
    let codepoints = collect_text_codepoints(&dom).unwrap();
    let expected: Vec<u32> = vec!['a', 'b', 'c'].into_iter().map(|c| c as u32).collect();
    assert_eq!(codepoints, expected);
  }

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
    selector_matches_with_custom_elements_defined(element, selector, true)
  }

  fn selector_matches_with_custom_elements_defined(
    element: &ElementRef,
    selector: &Selector<FastRenderSelectorImpl>,
    treat_custom_elements_as_defined: bool,
  ) -> bool {
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
    context.extra_data = ShadowMatchData::for_document()
      .with_custom_elements_defined(treat_custom_elements_as_defined)
      .with_sibling_cache(&sibling_cache);
    matches_selector(selector, 0, None, element, &mut context)
  }

  #[test]
  fn popover_open_pseudo_class_matches_open_popovers() {
    let popover = element_with_attrs(
      "div",
      vec![("popover", ""), ("data-fastr-open", "open")],
      vec![],
    );
    let closed = element_with_attrs(
      "div",
      vec![("popover", ""), ("data-fastr-open", "false")],
      vec![],
    );

    let selector = parse_selector("div:popover-open");
    let popover_ref = ElementRef::with_ancestors(&popover, &[]);
    let closed_ref = ElementRef::with_ancestors(&closed, &[]);
    assert!(selector_matches(&popover_ref, &selector));
    assert!(!selector_matches(&closed_ref, &selector));
  }

  #[test]
  fn modal_pseudo_class_matches_modal_dialogs() {
    let modal = element_with_attrs("dialog", vec![("data-fastr-open", "modal")], vec![]);
    let non_modal = element_with_attrs("dialog", vec![("open", "")], vec![]);

    let selector = parse_selector("dialog:modal");
    let modal_ref = ElementRef::with_ancestors(&modal, &[]);
    let non_modal_ref = ElementRef::with_ancestors(&non_modal, &[]);
    assert!(selector_matches(&modal_ref, &selector));
    assert!(!selector_matches(&non_modal_ref, &selector));
  }

  #[test]
  fn defined_pseudo_class_matches_custom_elements() {
    let node = element("details-dialog", vec![]);
    let node_ref = ElementRef::with_ancestors(&node, &[]);

    let selector = parse_selector("details-dialog:defined");
    assert!(selector_matches_with_custom_elements_defined(
      &node_ref, &selector, true
    ));
    assert!(!selector_matches_with_custom_elements_defined(
      &node_ref, &selector, false
    ));

    let selector = parse_selector("details-dialog:not(:defined)");
    assert!(!selector_matches_with_custom_elements_defined(
      &node_ref, &selector, true
    ));
    assert!(selector_matches_with_custom_elements_defined(
      &node_ref, &selector, false
    ));
  }

  #[test]
  fn root_element_has_no_element_parent() {
    let document = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![element("html", vec![element("body", vec![])])],
    };

    let html = &document.children[0];
    let html_ancestors: Vec<&DomNode> = vec![&document];
    let html_ref = ElementRef::with_ancestors(html, &html_ancestors);

    let universal_parent = parse_selector("* > html");
    assert!(
      !selector_matches(&html_ref, &universal_parent),
      "document parent should not satisfy element parent combinators"
    );

    let universal_ancestor = parse_selector("* html");
    assert!(
      !selector_matches(&html_ref, &universal_ancestor),
      "document ancestor should not satisfy element ancestor combinators"
    );

    let body = &html.children[0];
    let body_ancestors: Vec<&DomNode> = vec![&document, html];
    let body_ref = ElementRef::with_ancestors(body, &body_ancestors);

    let html_child_body = parse_selector("html > body");
    assert!(selector_matches(&body_ref, &html_child_body));
  }

  #[test]
  fn parent_element_skips_shadow_roots() {
    let shadow_child = element("span", vec![]);
    let shadow_root = DomNode {
      node_type: DomNodeType::ShadowRoot {
        mode: ShadowRootMode::Open,
        delegates_focus: false,
      },
      children: vec![shadow_child],
    };
    let host = element("div", vec![shadow_root]);

    let shadow_root_ref = &host.children[0];
    let shadow_child_ref = &shadow_root_ref.children[0];
    let shadow_ancestors: Vec<&DomNode> = vec![&host, shadow_root_ref];
    let shadow_element_ref = ElementRef::with_ancestors(shadow_child_ref, &shadow_ancestors);

    assert!(shadow_element_ref.parent_node_is_shadow_root());
    assert!(shadow_element_ref.parent_element().is_none());

    let normal_child = element("p", vec![]);
    let parent = element("section", vec![normal_child]);
    let normal_child_ref = &parent.children[0];
    let normal_ancestors: Vec<&DomNode> = vec![&parent];
    let normal_element_ref = ElementRef::with_ancestors(normal_child_ref, &normal_ancestors);

    let normal_parent = normal_element_ref
      .parent_element()
      .expect("normal elements should report element parents");
    assert_eq!(normal_parent.node.tag_name(), Some("section"));
    assert!(!normal_element_ref.parent_node_is_shadow_root());
  }

  #[test]
  fn descendant_selector_does_not_cross_shadow_root() {
    let shadow_child = element("span", vec![]);
    let shadow_root = DomNode {
      node_type: DomNodeType::ShadowRoot {
        mode: ShadowRootMode::Open,
        delegates_focus: false,
      },
      children: vec![shadow_child],
    };
    let host = element("div", vec![shadow_root]);
    let body = element("body", vec![host]);
    let html = element("html", vec![body]);
    let document = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![html],
    };

    let mut input = ParserInput::new("body span");
    let mut parser = Parser::new(&mut input);
    let selector_list =
      SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No).expect("parse");
    let selector = selector_list.slice().first().expect("expected a selector");

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

    let html = &document.children[0];
    let body = &html.children[0];
    let host = &body.children[0];
    let shadow_root = &host.children[0];
    let shadow_child = &shadow_root.children[0];
    let ancestors: Vec<&DomNode> = vec![&document, html, body, host, shadow_root];
    let element_ref = ElementRef::with_ancestors(shadow_child, &ancestors);

    assert!(
      !matches_selector(selector, 0, None, &element_ref, &mut context),
      "elements in shadow trees should not match selectors that rely on light DOM ancestors"
    );
  }

  #[test]
  fn namespace_matching_defaults_to_html() {
    let node = element("div", vec![]);
    let element_ref = ElementRef::new(&node);

    assert!(element_ref.has_namespace(""));
    assert!(element_ref.has_namespace(HTML_NAMESPACE));
    assert!(!element_ref.has_namespace("http://www.w3.org/2000/svg"));
  }

  #[test]
  fn root_matches_html_case_insensitive() {
    let upper = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "HTML".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };

    let svg_root = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "svg".to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };

    assert!(matches(&upper, &[], &PseudoClass::Root));
    assert!(!matches(&svg_root, &[], &PseudoClass::Root));
  }

  #[test]
  fn scope_matches_document_root_without_anchor() {
    let document = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![element("html", vec![element("body", vec![])])],
    };

    let html = &document.children[0];
    let body = &document.children[0].children[0];

    assert!(matches(html, &[&document], &PseudoClass::Scope));
    assert!(!matches(body, &[&document, html], &PseudoClass::Scope));
  }

  #[test]
  fn is_same_type_ignores_ascii_case() {
    let upper = element("DIV", vec![]);
    let lower = element("div", vec![]);

    let upper_ref = ElementRef::new(&upper);
    let lower_ref = ElementRef::new(&lower);

    assert!(upper_ref.is_same_type(&lower_ref));
  }

  #[test]
  fn is_same_type_accounts_for_namespace() {
    let html_div = element("div", vec![]);
    let svg_div = svg_element("div");

    let html_ref = ElementRef::new(&html_div);
    let svg_ref = ElementRef::new(&svg_div);

    assert!(!html_ref.is_same_type(&svg_ref));
  }

  #[test]
  fn has_local_name_respects_case_for_foreign_elements() {
    let svg = svg_element("linearGradient");
    let svg_ref = ElementRef::new(&svg);

    assert!(svg_ref.has_local_name("linearGradient"));
    assert!(!svg_ref.has_local_name("lineargradient"));
    assert!(!svg_ref.has_local_name("LINEARGRADIENT"));
  }

  #[test]
  fn namespace_matching_uses_element_namespace() {
    let svg = svg_element("svg");
    let svg_ref = ElementRef::new(&svg);

    assert!(svg_ref.has_namespace(""));
    assert!(svg_ref.has_namespace(SVG_NAMESPACE));
    assert!(!svg_ref.has_namespace(HTML_NAMESPACE));
  }

  #[test]
  fn is_part_checks_whitespace_tokens() {
    let node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("part".to_string(), "header body\tfooter".to_string())],
      },
      children: vec![],
    };
    let element = ElementRef::new(&node);
    assert!(element.is_part(&CssString::from("header")));
    assert!(element.is_part(&CssString::from("footer")));
    assert!(!element.is_part(&CssString::from("aside")));
  }

  #[test]
  fn imported_part_handles_aliases_and_identity() {
    let node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("exportparts".to_string(), "label, inner:outer".to_string())],
      },
      children: vec![],
    };
    let element = ElementRef::new(&node);

    assert_eq!(
      element.imported_part(&CssString::from("label")),
      Some(CssString::from("label"))
    );
    assert_eq!(
      element.imported_part(&CssString::from("outer")),
      Some(CssString::from("inner"))
    );
    assert_eq!(element.imported_part(&CssString::from("missing")), None);
  }

  #[test]
  fn non_ascii_whitespace_imported_part_does_not_trim_nbsp_in_exportparts() {
    let nbsp = "\u{00A0}";
    let exportparts = format!("{nbsp}label");
    let node = element_with_attrs(
      "div",
      vec![("exportparts", exportparts.as_str())],
      vec![],
    );
    let element = ElementRef::new(&node);
    assert_eq!(
      element.imported_part(&CssString::from("label")),
      None,
      "NBSP must not be treated as whitespace when parsing exportparts"
    );
  }

  #[test]
  fn parse_exportparts_supports_pseudo_element_mappings() {
    assert_eq!(
      parse_exportparts("::before : preceding-text, ::after:following-text"),
      vec![
        ("::before".to_string(), "preceding-text".to_string()),
        ("::after".to_string(), "following-text".to_string()),
      ]
    );
  }

  #[test]
  fn parse_exportparts_ignores_missing_outer_ident_after_colon() {
    assert_eq!(parse_exportparts("label:"), Vec::<(String, String)>::new());
    assert_eq!(
      parse_exportparts("label:outer, other:"),
      vec![("label".to_string(), "outer".to_string())],
    );
  }

  fn collect_wbr_texts(node: &DomNode, out: &mut Vec<String>) {
    if let DomNodeType::Element { tag_name, .. } = &node.node_type {
      if tag_name.eq_ignore_ascii_case("wbr") {
        for child in node.children.iter() {
          if let DomNodeType::Text { content } = &child.node_type {
            out.push(content.clone());
          }
        }
      }
    }
    for child in node.children.iter() {
      collect_wbr_texts(child, out);
    }
  }

  #[test]
  fn attribute_lookup_is_case_insensitive() {
    let node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("HREF".to_string(), "foo".to_string())],
      },
      children: vec![],
    };

    assert_eq!(node.get_attribute("href"), Some("foo".to_string()));
    assert_eq!(node.get_attribute("HRef"), Some("foo".to_string()));
  }

  #[test]
  fn attr_selector_respects_case_sensitivity() {
    use selectors::attr::AttrSelectorOperation;
    use selectors::attr::AttrSelectorOperator;
    use selectors::attr::NamespaceConstraint;

    let node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("foo".to_string(), "Bar".to_string())],
      },
      children: vec![],
    };
    let element_ref = ElementRef::new(&node);
    let local = CssString("foo".into());

    let value_insensitive = CssString("bar".into());
    let op_insensitive = AttrSelectorOperation::WithValue {
      operator: AttrSelectorOperator::Equal,
      case_sensitivity: CaseSensitivity::AsciiCaseInsensitive,
      value: &value_insensitive,
    };
    assert!(element_ref.attr_matches(&NamespaceConstraint::Any, &local, &op_insensitive));

    let value_sensitive = CssString("bar".into());
    let op_sensitive = AttrSelectorOperation::WithValue {
      operator: AttrSelectorOperator::Equal,
      case_sensitivity: CaseSensitivity::CaseSensitive,
      value: &value_sensitive,
    };
    assert!(!element_ref.attr_matches(&NamespaceConstraint::Any, &local, &op_sensitive));

    // Namespaced selector should fail when requesting a non-HTML namespace.
    let svg_ns = CssString("http://www.w3.org/2000/svg".into());
    assert!(!element_ref.attr_matches(
      &NamespaceConstraint::Specific(&svg_ns),
      &local,
      &op_insensitive,
    ));
  }

  #[test]
  fn id_and_class_respect_case_sensitivity() {
    let node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("id".to_string(), "Foo".to_string()),
          ("class".to_string(), "Bar baz".to_string()),
        ],
      },
      children: vec![],
    };
    let element_ref = ElementRef::new(&node);

    assert!(element_ref.has_id(&CssString("Foo".into()), CaseSensitivity::CaseSensitive));
    assert!(!element_ref.has_id(&CssString("foo".into()), CaseSensitivity::CaseSensitive));
    assert!(element_ref.has_id(
      &CssString("foo".into()),
      CaseSensitivity::AsciiCaseInsensitive
    ));

    assert!(element_ref.has_class(&CssString("Bar".into()), CaseSensitivity::CaseSensitive));
    assert!(!element_ref.has_class(&CssString("bar".into()), CaseSensitivity::CaseSensitive));
    assert!(element_ref.has_class(
      &CssString("bar".into()),
      CaseSensitivity::AsciiCaseInsensitive
    ));
  }

  #[test]
  fn has_class_matches_with_and_without_attr_cache() {
    let node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("class".to_string(), "a B\tc".to_string())],
      },
      children: vec![],
    };

    let cache = ElementAttrCache::new(0);
    let cached = ElementRef::new(&node).with_attr_cache(Some(&cache));
    let uncached = ElementRef::new(&node);

    let a = CssString::from("a");
    assert!(uncached.has_class(&a, CaseSensitivity::CaseSensitive));
    assert_eq!(
      uncached.has_class(&a, CaseSensitivity::CaseSensitive),
      cached.has_class(&a, CaseSensitivity::CaseSensitive)
    );

    let b_lower = CssString::from("b");
    assert!(!uncached.has_class(&b_lower, CaseSensitivity::CaseSensitive));
    assert_eq!(
      uncached.has_class(&b_lower, CaseSensitivity::CaseSensitive),
      cached.has_class(&b_lower, CaseSensitivity::CaseSensitive)
    );
    assert!(uncached.has_class(&b_lower, CaseSensitivity::AsciiCaseInsensitive));
    assert_eq!(
      uncached.has_class(&b_lower, CaseSensitivity::AsciiCaseInsensitive),
      cached.has_class(&b_lower, CaseSensitivity::AsciiCaseInsensitive)
    );

    let missing = CssString::from("missing");
    assert!(!uncached.has_class(&missing, CaseSensitivity::AsciiCaseInsensitive));
    assert_eq!(
      uncached.has_class(&missing, CaseSensitivity::AsciiCaseInsensitive),
      cached.has_class(&missing, CaseSensitivity::AsciiCaseInsensitive)
    );
  }

  #[test]
  fn has_class_large_token_list_matches_with_and_without_attr_cache() {
    let node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("class".to_string(), "c0 C1 c2 C3 c4 C5 c6 C7".to_string())],
      },
      children: vec![],
    };

    let cache = ElementAttrCache::new(0);
    let cached = ElementRef::new(&node).with_attr_cache(Some(&cache));
    let uncached = ElementRef::new(&node);

    let query = CssString::from("c3");
    assert!(!uncached.has_class(&query, CaseSensitivity::CaseSensitive));
    assert!(uncached.has_class(&query, CaseSensitivity::AsciiCaseInsensitive));
    assert_eq!(
      uncached.has_class(&query, CaseSensitivity::CaseSensitive),
      cached.has_class(&query, CaseSensitivity::CaseSensitive)
    );
    assert_eq!(
      uncached.has_class(&query, CaseSensitivity::AsciiCaseInsensitive),
      cached.has_class(&query, CaseSensitivity::AsciiCaseInsensitive)
    );
  }

  #[test]
  fn attr_matches_name_casing_matches_with_and_without_attr_cache() {
    use selectors::attr::AttrSelectorOperation;
    use selectors::attr::NamespaceConstraint;

    let ns: NamespaceConstraint<&CssString> = NamespaceConstraint::Any;
    let op: AttrSelectorOperation<&CssString> = AttrSelectorOperation::Exists;

    let mut html_attrs = vec![("DaTa-X".to_string(), "1".to_string())];
    for idx in 0..12 {
      html_attrs.push((format!("data-a{idx}"), "x".to_string()));
    }
    let html_node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: html_attrs,
      },
      children: vec![],
    };

    let html_cache = ElementAttrCache::new(0);
    let html_cached = ElementRef::new(&html_node).with_attr_cache(Some(&html_cache));
    let html_uncached = ElementRef::new(&html_node);
    let data_x = CssString::from("data-x");
    assert!(html_uncached.attr_matches(&ns, &data_x, &op));
    assert_eq!(
      html_uncached.attr_matches(&ns, &data_x, &op),
      html_cached.attr_matches(&ns, &data_x, &op)
    );

    let mut svg_attrs = vec![("viewBox".to_string(), "0 0 10 10".to_string())];
    for idx in 0..12 {
      svg_attrs.push((format!("attr{idx}"), "y".to_string()));
    }
    let svg_node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "svg".to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: svg_attrs,
      },
      children: vec![],
    };

    let svg_cache = ElementAttrCache::new(0);
    let svg_cached = ElementRef::new(&svg_node).with_attr_cache(Some(&svg_cache));
    let svg_uncached = ElementRef::new(&svg_node);
    let viewbox_lower = CssString::from("viewbox");
    assert!(!svg_uncached.attr_matches(&ns, &viewbox_lower, &op));
    assert_eq!(
      svg_uncached.attr_matches(&ns, &viewbox_lower, &op),
      svg_cached.attr_matches(&ns, &viewbox_lower, &op)
    );
    let viewbox = CssString::from("viewBox");
    assert!(svg_uncached.attr_matches(&ns, &viewbox, &op));
    assert_eq!(
      svg_uncached.attr_matches(&ns, &viewbox, &op),
      svg_cached.attr_matches(&ns, &viewbox, &op)
    );
  }

  #[test]
  fn empty_pseudo_requires_no_element_or_text_children() {
    let empty = element("div", vec![]);
    let whitespace = element("div", vec![text(" \n")]);
    let child = element("div", vec![element("span", vec![])]);

    assert!(matches(&empty, &[], &PseudoClass::Empty));
    assert!(!matches(&whitespace, &[], &PseudoClass::Empty));
    assert!(!matches(&child, &[], &PseudoClass::Empty));
  }

  #[test]
  fn wbr_inserts_zero_width_break_text_node() {
    let dom = parse_html("<p>Hello<wbr>World</p>").expect("parse html");
    let mut texts = Vec::new();
    collect_wbr_texts(&dom, &mut texts);
    assert!(texts.iter().any(|t| t == "\u{200B}"));
  }

  #[test]
  fn type_position_pseudos_filter_by_tag_name() {
    let parent = element(
      "div",
      vec![
        element("span", vec![]),
        element("em", vec![]),
        element("span", vec![]),
      ],
    );
    let ancestors: Vec<&DomNode> = vec![&parent];

    let first_span = &parent.children[0];
    let em = &parent.children[1];
    let second_span = &parent.children[2];

    assert!(matches(first_span, &ancestors, &PseudoClass::FirstOfType));
    assert!(!matches(first_span, &ancestors, &PseudoClass::LastOfType));
    assert!(!matches(first_span, &ancestors, &PseudoClass::OnlyOfType));
    assert!(!matches(
      first_span,
      &ancestors,
      &PseudoClass::NthOfType(2, 0)
    ));
    assert!(matches(
      first_span,
      &ancestors,
      &PseudoClass::NthLastOfType(2, 0)
    ));

    assert!(matches(second_span, &ancestors, &PseudoClass::LastOfType));
    assert!(!matches(second_span, &ancestors, &PseudoClass::OnlyOfType));
    assert!(matches(
      second_span,
      &ancestors,
      &PseudoClass::NthOfType(2, 0)
    ));
    assert!(matches(
      second_span,
      &ancestors,
      &PseudoClass::NthLastOfType(0, 1)
    ));

    // Different element type should be unaffected by span counting.
    assert!(matches(em, &ancestors, &PseudoClass::OnlyOfType));
  }

  #[test]
  fn only_of_type_ignores_unrelated_siblings() {
    let parent = element("div", vec![element("span", vec![]), element("div", vec![])]);
    let ancestors: Vec<&DomNode> = vec![&parent];

    let span = &parent.children[0];
    let div = &parent.children[1];

    assert!(matches(span, &ancestors, &PseudoClass::OnlyOfType));
    assert!(matches(div, &ancestors, &PseudoClass::OnlyOfType));
  }

  #[test]
  fn lang_matches_inherit_and_prefix() {
    let child = element("p", vec![]);
    let root = element(
      "html",
      vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("lang".to_string(), "en-US".to_string())],
        },
        children: vec![child],
      }],
    );

    let ancestors: Vec<&DomNode> = vec![&root, &root.children[0]];
    let node = &root.children[0].children[0];

    assert!(matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["en".into()])
    ));
    assert!(matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["en-us".into()])
    ));
    assert!(matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["*".into()])
    ));
    assert!(!matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["fr".into()])
    ));

    // Multiple ranges OR together
    assert!(matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["fr".into(), "en".into()])
    ));
  }

  #[test]
  fn lang_matches_normalizes_underscores_and_whitespace() {
    let child = element("p", vec![]);
    let container = element_with_attrs("div", vec![("lang", " sr_Cyrl_RS ")], vec![child]);
    let root = element("html", vec![container]);
    let ancestors: Vec<&DomNode> = vec![&root, &root.children[0]];
    let node = &root.children[0].children[0];

    assert!(matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["sr".into()])
    ));
    assert!(matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["sr-cyrl".into()])
    ));
    assert!(matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["sr-cyrl-rs".into()])
    ));

    assert!(!matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["en".into()])
    ));
    assert!(!matches(
      node,
      &ancestors,
      &PseudoClass::Lang(vec!["sr-latn".into()])
    ));
  }

  #[test]
  fn dir_matches_inherited_direction() {
    let child = element("span", vec![]);
    let root = element(
      "div",
      vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "p".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("dir".to_string(), "rtl".to_string())],
        },
        children: vec![child],
      }],
    );
    let ancestors: Vec<&DomNode> = vec![&root, &root.children[0]];
    let node = &root.children[0].children[0];

    assert!(matches(
      node,
      &ancestors,
      &PseudoClass::Dir(TextDirection::Rtl)
    ));
    assert!(!matches(
      node,
      &ancestors,
      &PseudoClass::Dir(TextDirection::Ltr)
    ));
  }

  #[test]
  fn dir_auto_uses_first_strong() {
    let rtl_text = DomNode {
      node_type: DomNodeType::Text {
        content: "שלום".to_string(),
      },
      children: vec![],
    };
    let root = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("dir".to_string(), "auto".to_string())],
      },
      children: vec![rtl_text],
    };
    assert!(matches(&root, &[], &PseudoClass::Dir(TextDirection::Rtl)));
    assert!(!matches(&root, &[], &PseudoClass::Dir(TextDirection::Ltr)));
  }

  #[test]
  fn dir_auto_on_ancestor_inherits_resolved_direction() {
    let rtl_text = DomNode {
      node_type: DomNodeType::Text {
        content: "שלום".to_string(),
      },
      children: vec![],
    };
    let child = element("span", vec![]);
    let container = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "p".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("dir".to_string(), "auto".to_string())],
      },
      children: vec![rtl_text, child],
    };
    let root = element("div", vec![container]);
    let ancestors: Vec<&DomNode> = vec![&root, &root.children[0]];
    let target = &root.children[0].children[1];
    assert!(matches(
      target,
      &ancestors,
      &PseudoClass::Dir(TextDirection::Rtl)
    ));
  }

  #[test]
  fn dir_auto_ignores_template_contents() {
    let template = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "template".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![text("שלום")],
    };
    let root = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("dir".to_string(), "auto".to_string())],
      },
      children: vec![template],
    };

    assert_eq!(resolve_first_strong_direction(&root), None);
    assert!(matches(&root, &[], &PseudoClass::Dir(TextDirection::Ltr)));
    assert!(!matches(&root, &[], &PseudoClass::Dir(TextDirection::Rtl)));
  }

  #[test]
  fn any_link_matches_href_anchors() {
    let link = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("href".to_string(), "#foo".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&link, &[], &PseudoClass::AnyLink));
    assert!(matches(&link, &[], &PseudoClass::Link));
    assert!(!matches(&link, &[], &PseudoClass::Visited));

    // Area and link elements also qualify
    let area = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "area".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("href".to_string(), "/foo".to_string())],
      },
      children: vec![],
    };
    let stylesheet_link = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "link".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("href".to_string(), "style.css".to_string())],
      },
      children: vec![],
    };

    assert!(matches(&area, &[], &PseudoClass::AnyLink));
    assert!(matches(&stylesheet_link, &[], &PseudoClass::AnyLink));
  }

  #[test]
  fn placeholder_shown_matches_empty_controls() {
    let input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("placeholder".to_string(), "Search".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&input, &[], &PseudoClass::PlaceholderShown));

    let with_value = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("placeholder".to_string(), "Search".to_string()),
          ("value".to_string(), "query".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(!matches(&with_value, &[], &PseudoClass::PlaceholderShown));

    let checkbox = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "checkbox".to_string()),
          ("placeholder".to_string(), "X".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(!matches(&checkbox, &[], &PseudoClass::PlaceholderShown));

    let empty_textarea = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "textarea".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("placeholder".to_string(), "Describe".to_string())],
      },
      children: vec![],
    };
    assert!(matches(
      &empty_textarea,
      &[],
      &PseudoClass::PlaceholderShown
    ));

    let newline_only_textarea = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "textarea".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("placeholder".to_string(), "Describe".to_string())],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Text {
          content: "\n".to_string(),
        },
        children: vec![],
      }],
    };
    assert!(matches(
      &newline_only_textarea,
      &[],
      &PseudoClass::PlaceholderShown
    ));

    let prefilled_textarea = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "textarea".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("placeholder".to_string(), "Describe".to_string())],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Text {
          content: "Hello".to_string(),
        },
        children: vec![],
      }],
    };
    assert!(!matches(
      &prefilled_textarea,
      &[],
      &PseudoClass::PlaceholderShown
    ));

    let formatted_prefilled_textarea = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "textarea".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("placeholder".to_string(), "Describe".to_string())],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Text {
          content: "\nHello".to_string(),
        },
        children: vec![],
      }],
    };
    assert!(!matches(
      &formatted_prefilled_textarea,
      &[],
      &PseudoClass::PlaceholderShown
    ));
  }

  #[test]
  fn legacy_vendor_placeholder_pseudos_inside_not_match_like_placeholder_shown() {
    let placeholder_empty = element_with_attrs(
      "input",
      vec![("placeholder", "x"), ("value", "")],
      vec![],
    );
    let placeholder_empty_ref = ElementRef::new(&placeholder_empty);

    let placeholder_filled = element_with_attrs(
      "input",
      vec![("placeholder", "x"), ("value", "hello")],
      vec![],
    );
    let placeholder_filled_ref = ElementRef::new(&placeholder_filled);

    for pseudo in [
      ":-webkit-input-placeholder",
      ":-moz-placeholder",
      ":-ms-input-placeholder",
    ] {
      let selector = parse_selector(&format!("input:not({pseudo})"));
      assert!(
        !selector_matches(&placeholder_empty_ref, &selector),
        "input:not({pseudo}) should not match when placeholder is shown"
      );
      assert!(
        selector_matches(&placeholder_filled_ref, &selector),
        "input:not({pseudo}) should match when control has a value"
      );
    }
  }

  #[test]
  fn autofill_never_matches_without_state() {
    let input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "text".to_string()),
          ("value".to_string(), "filled".to_string()),
        ],
      },
      children: vec![],
    };

    assert!(!matches(&input, &[], &PseudoClass::Autofill));
  }

  #[test]
  fn required_and_optional_match_supported_controls() {
    let text_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("type".to_string(), "text".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&text_input, &[], &PseudoClass::Optional));
    assert!(!matches(&text_input, &[], &PseudoClass::Required));

    let required_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "email".to_string()),
          ("required".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&required_input, &[], &PseudoClass::Required));
    assert!(!matches(&required_input, &[], &PseudoClass::Optional));

    let disabled_required = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("disabled".to_string(), "disabled".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(
      matches(&disabled_required, &[], &PseudoClass::Required),
      "disabled required controls still match :required"
    );
    assert!(!matches(&disabled_required, &[], &PseudoClass::Optional));

    let submit_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "submit".to_string()),
          ("required".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(!matches(&submit_input, &[], &PseudoClass::Required));
    assert!(!matches(&submit_input, &[], &PseudoClass::Optional));

    let range_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("type".to_string(), "range".to_string())],
      },
      children: vec![],
    };
    assert!(
      !matches(&range_input, &[], &PseudoClass::Required),
      "required does not apply to range inputs"
    );
    assert!(
      !matches(&range_input, &[], &PseudoClass::Optional),
      "optional does not apply to range inputs"
    );

    let color_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "color".to_string()),
          ("required".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(
      !matches(&color_input, &[], &PseudoClass::Required),
      "required does not apply to color inputs"
    );
    assert!(
      !matches(&color_input, &[], &PseudoClass::Optional),
      "optional does not apply to color inputs"
    );

    let select = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("required".to_string(), "required".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&select, &[], &PseudoClass::Required));

    let fieldset = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "fieldset".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("disabled".to_string(), "true".to_string())],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "input".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![
            ("type".to_string(), "text".to_string()),
            ("required".to_string(), "true".to_string()),
          ],
        },
        children: vec![],
      }],
    };
    let ancestors: Vec<&DomNode> = vec![&fieldset];
    let child = &fieldset.children[0];
    assert!(
      matches(child, &ancestors, &PseudoClass::Required),
      "fieldset-disabled controls still match :required"
    );
    assert!(!matches(child, &ancestors, &PseudoClass::Optional));
  }

  #[test]
  fn range_inputs_match_in_range_by_default() {
    let input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "range".to_string()),
          ("min".to_string(), "0".to_string()),
          ("max".to_string(), "10".to_string()),
        ],
      },
      children: vec![],
    };

    assert!(matches(&input, &[], &PseudoClass::InRange));
    assert!(!matches(&input, &[], &PseudoClass::OutOfRange));
  }

  #[test]
  fn range_values_are_clamped_before_range_state_checks() {
    let input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "range".to_string()),
          ("min".to_string(), "0".to_string()),
          ("max".to_string(), "10".to_string()),
          ("value".to_string(), "20".to_string()),
        ],
      },
      children: vec![],
    };

    // The HTML value sanitization algorithm clamps the current value into the [min, max] range,
    // so this remains in-range despite the authored value being too high.
    assert!(matches(&input, &[], &PseudoClass::InRange));
    assert!(!matches(&input, &[], &PseudoClass::OutOfRange));
  }

  #[test]
  fn link_and_visited_match_state_flags() {
    let unvisited = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("href".to_string(), "https://example.com".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&unvisited, &[], &PseudoClass::Link));
    assert!(!matches(&unvisited, &[], &PseudoClass::Visited));

    let visited = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("href".to_string(), "https://example.com".to_string()),
          ("data-fastr-visited".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(!matches(&visited, &[], &PseudoClass::Link));
    assert!(matches(&visited, &[], &PseudoClass::Visited));
  }

  #[test]
  fn link_pseudo_classes_match_case_insensitive_tag_names() {
    let unvisited = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "A".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("href".to_string(), "https://example.com".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&unvisited, &[], &PseudoClass::AnyLink));
    assert!(matches(&unvisited, &[], &PseudoClass::Link));
    assert!(!matches(&unvisited, &[], &PseudoClass::Visited));

    let visited = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "A".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("href".to_string(), "https://example.com".to_string()),
          ("data-fastr-visited".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(!matches(&visited, &[], &PseudoClass::Link));
    assert!(matches(&visited, &[], &PseudoClass::Visited));
  }

  #[test]
  fn active_matches_when_flagged() {
    let inactive = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("href".to_string(), "https://example.com".to_string())],
      },
      children: vec![],
    };
    assert!(!matches(&inactive, &[], &PseudoClass::Active));

    let active = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("href".to_string(), "https://example.com".to_string()),
          ("data-fastr-active".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&active, &[], &PseudoClass::Active));
  }

  #[test]
  fn hover_and_focus_match_when_flagged() {
    let hover = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("href".to_string(), "https://example.com".to_string()),
          ("data-fastr-hover".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&hover, &[], &PseudoClass::Hover));
    assert!(!matches(&hover, &[], &PseudoClass::Focus));

    let focus = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("href".to_string(), "https://example.com".to_string()),
          ("data-fastr-focus".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(!matches(&focus, &[], &PseudoClass::Hover));
    assert!(matches(&focus, &[], &PseudoClass::Focus));
  }

  #[test]
  fn hover_and_focus_do_not_match_by_default() {
    let link = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("href".to_string(), "https://example.com".to_string())],
      },
      children: vec![],
    };

    assert!(!matches(&link, &[], &PseudoClass::Hover));
    assert!(!matches(&link, &[], &PseudoClass::Focus));
  }

  #[test]
  fn svg_is_not_focusable_by_default() {
    let svg = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "svg".to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: vec![("data-fastr-focus".to_string(), "true".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&svg, &[], &PseudoClass::Hover) == false);
    assert!(!matches(&svg, &[], &PseudoClass::Focus));
  }

  #[test]
  fn svg_focusable_true_allows_focus() {
    let svg = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "svg".to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: vec![
          ("focusable".to_string(), "true".to_string()),
          ("data-fastr-focus".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&svg, &[], &PseudoClass::Focus));
  }

  #[test]
  fn svg_focusable_false_blocks_focus_even_when_flagged() {
    let svg = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "svg".to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: vec![
          ("focusable".to_string(), "false".to_string()),
          ("data-fastr-focus".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };

    assert!(!matches(&svg, &[], &PseudoClass::Focus));
  }

  #[test]
  fn focus_within_matches_focused_element() {
    let focused = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("data-fastr-focus".to_string(), "true".to_string())],
      },
      children: vec![],
    };

    assert!(matches(&focused, &[], &PseudoClass::FocusWithin));
  }

  #[test]
  fn focus_within_matches_descendant_focus() {
    let mut parent = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };

    let child = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "button".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("data-fastr-focus".to_string(), "true".to_string())],
      },
      children: vec![],
    };

    parent.children.push(child);

    assert!(matches(&parent, &[], &PseudoClass::FocusWithin));
  }

  #[test]
  fn focus_within_respects_svg_focusable() {
    let mut parent = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };

    let svg_unfocusable = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "svg".to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: vec![("data-fastr-focus".to_string(), "true".to_string())],
      },
      children: vec![],
    };

    parent.children.push(svg_unfocusable);

    assert!(!matches(&parent, &[], &PseudoClass::FocusWithin));

    let mut parent_focusable = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };

    let svg_focusable = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "svg".to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: vec![
          ("focusable".to_string(), "true".to_string()),
          ("data-fastr-focus".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };

    parent_focusable.children.push(svg_focusable);

    assert!(matches(&parent_focusable, &[], &PseudoClass::FocusWithin));
  }

  #[test]
  fn focus_visible_matches_when_flagged() {
    let dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "button".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("data-fastr-focus".to_string(), "true".to_string()),
          ("data-fastr-focus-visible".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };

    assert!(matches(&dom, &[], &PseudoClass::FocusVisible));
  }

  #[test]
  fn focus_visible_requires_visible_flag() {
    let dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "button".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("data-fastr-focus".to_string(), "true".to_string())],
      },
      children: vec![],
    };

    assert!(!matches(&dom, &[], &PseudoClass::FocusVisible));
  }

  #[test]
  fn checked_matches_inputs_and_options() {
    let checkbox = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "checkbox".to_string()),
          ("checked".to_string(), "checked".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&checkbox, &[], &PseudoClass::Checked));

    let radio = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("type".to_string(), "radio".to_string())],
      },
      children: vec![],
    };
    assert!(!matches(&radio, &[], &PseudoClass::Checked));

    let option_selected = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "option".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("selected".to_string(), "selected".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&option_selected, &[], &PseudoClass::Checked));

    let select = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select];
    let first = &select.children[0];
    let second = &select.children[1];
    assert!(matches(first, &ancestors, &PseudoClass::Checked));
    assert!(!matches(second, &ancestors, &PseudoClass::Checked));

    let select_with_explicit = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("selected".to_string(), "selected".to_string())],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select_with_explicit];
    let first = &select_with_explicit.children[0];
    let second = &select_with_explicit.children[1];
    assert!(!matches(first, &ancestors, &PseudoClass::Checked));
    assert!(matches(second, &ancestors, &PseudoClass::Checked));

    let select_disabled_selected_placeholder = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![
            ("disabled".to_string(), "disabled".to_string()),
            ("selected".to_string(), "selected".to_string()),
            ("value".to_string(), String::new()),
          ],
        },
        children: vec![DomNode {
          node_type: DomNodeType::Text {
            content: "Placeholder".to_string(),
          },
          children: vec![],
        }],
      }],
    };
    let ancestors: Vec<&DomNode> = vec![&select_disabled_selected_placeholder];
    let placeholder = &select_disabled_selected_placeholder.children[0];
    assert!(
      matches(placeholder, &ancestors, &PseudoClass::Checked),
      "disabled selected placeholder should still be :checked"
    );

    let select_two_selected = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("selected".to_string(), "selected".to_string())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("selected".to_string(), "selected".to_string())],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select_two_selected];
    let first = &select_two_selected.children[0];
    let second = &select_two_selected.children[1];
    assert!(
      !matches(first, &ancestors, &PseudoClass::Checked),
      "single-select should only select the last <option selected> in tree order"
    );
    assert!(matches(second, &ancestors, &PseudoClass::Checked));

    let select_disabled_first = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("disabled".to_string(), "disabled".to_string())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select_disabled_first];
    let first = &select_disabled_first.children[0];
    let second = &select_disabled_first.children[1];
    assert!(!matches(first, &ancestors, &PseudoClass::Checked));
    assert!(matches(second, &ancestors, &PseudoClass::Checked));

    let select_all_disabled = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("disabled".to_string(), "disabled".to_string())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("disabled".to_string(), "disabled".to_string())],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select_all_disabled];
    let first = &select_all_disabled.children[0];
    let second = &select_all_disabled.children[1];
    assert!(matches(first, &ancestors, &PseudoClass::Checked));
    assert!(!matches(second, &ancestors, &PseudoClass::Checked));

    let select_disabled_selected_placeholder = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![
              ("disabled".to_string(), "disabled".to_string()),
              ("selected".to_string(), "selected".to_string()),
            ],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select_disabled_selected_placeholder];
    let first = &select_disabled_selected_placeholder.children[0];
    let second = &select_disabled_selected_placeholder.children[1];
    assert!(matches(first, &ancestors, &PseudoClass::Checked));
    assert!(!matches(second, &ancestors, &PseudoClass::Checked));

    let select_multiple = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("multiple".to_string(), "multiple".to_string())],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      }],
    };
    let ancestors: Vec<&DomNode> = vec![&select_multiple];
    let only_option = &select_multiple.children[0];
    assert!(!matches(only_option, &ancestors, &PseudoClass::Checked));

    let select_multiple_selected = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("multiple".to_string(), "multiple".to_string())],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("selected".to_string(), "selected".to_string())],
        },
        children: vec![],
      }],
    };
    let ancestors: Vec<&DomNode> = vec![&select_multiple_selected];
    let selected_option = &select_multiple_selected.children[0];
    assert!(matches(selected_option, &ancestors, &PseudoClass::Checked));

    let select_all_disabled = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("disabled".to_string(), "disabled".to_string())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("disabled".to_string(), "disabled".to_string())],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select_all_disabled];
    let first = &select_all_disabled.children[0];
    let second = &select_all_disabled.children[1];
    assert!(
      matches(first, &ancestors, &PseudoClass::Checked),
      "single-select with all options disabled should default to the first option"
    );
    assert!(!matches(second, &ancestors, &PseudoClass::Checked));

    let select_hidden_selected = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![
              ("hidden".to_string(), "hidden".to_string()),
              ("selected".to_string(), "selected".to_string()),
            ],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select_hidden_selected];
    let hidden_option = &select_hidden_selected.children[0];
    let visible_option = &select_hidden_selected.children[1];
    assert!(!matches(hidden_option, &ancestors, &PseudoClass::Checked));
    assert!(matches(visible_option, &ancestors, &PseudoClass::Checked));

    let select_hidden_optgroup = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "optgroup".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("hidden".to_string(), "hidden".to_string())],
          },
          children: vec![DomNode {
            node_type: DomNodeType::Element {
              tag_name: "option".to_string(),
              namespace: HTML_NAMESPACE.to_string(),
              attributes: vec![],
            },
            children: vec![],
          }],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
      ],
    };
    let optgroup = &select_hidden_optgroup.children[0];
    let hidden_optgroup_option = &optgroup.children[0];
    let visible_option = &select_hidden_optgroup.children[1];
    let ancestors: Vec<&DomNode> = vec![&select_hidden_optgroup, optgroup];
    assert!(!matches(
      hidden_optgroup_option,
      &ancestors,
      &PseudoClass::Checked
    ));
    let ancestors: Vec<&DomNode> = vec![&select_hidden_optgroup];
    assert!(matches(visible_option, &ancestors, &PseudoClass::Checked));
  }

  #[test]
  fn radio_group_checked_only_matches_last_checked_in_tree_order() {
    let form = element(
      "form",
      vec![
        element_with_attrs(
          "input",
          vec![("type", "radio"), ("name", "group"), ("checked", "")],
          vec![],
        ),
        element_with_attrs(
          "input",
          vec![("type", "radio"), ("name", "group"), ("checked", "")],
          vec![],
        ),
      ],
    );
    let first = &form.children[0];
    let second = &form.children[1];
    let ancestors: Vec<&DomNode> = vec![&form];

    assert!(!matches(first, &ancestors, &PseudoClass::Checked));
    assert!(matches(second, &ancestors, &PseudoClass::Checked));
  }

  #[test]
  fn radio_without_name_is_not_mutually_exclusive_for_checkedness() {
    let form = element(
      "form",
      vec![
        element_with_attrs("input", vec![("type", "radio"), ("checked", "")], vec![]),
        element_with_attrs("input", vec![("type", "radio"), ("checked", "")], vec![]),
      ],
    );
    let first = &form.children[0];
    let second = &form.children[1];
    let ancestors: Vec<&DomNode> = vec![&form];

    assert!(matches(first, &ancestors, &PseudoClass::Checked));
    assert!(matches(second, &ancestors, &PseudoClass::Checked));
  }

  #[test]
  fn required_radio_validity_is_satisfied_by_any_checked_radio_in_group() {
    let form_checked = element(
      "form",
      vec![
        element_with_attrs(
          "input",
          vec![("type", "radio"), ("name", "group"), ("required", "")],
          vec![],
        ),
        element_with_attrs(
          "input",
          vec![("type", "radio"), ("name", "group"), ("checked", "")],
          vec![],
        ),
      ],
    );
    let required_radio = &form_checked.children[0];
    let ancestors: Vec<&DomNode> = vec![&form_checked];
    assert!(matches(required_radio, &ancestors, &PseudoClass::Valid));
    assert!(!matches(required_radio, &ancestors, &PseudoClass::Invalid));

    let form_unchecked = element(
      "form",
      vec![
        element_with_attrs(
          "input",
          vec![("type", "radio"), ("name", "group"), ("required", "")],
          vec![],
        ),
        element_with_attrs("input", vec![("type", "radio"), ("name", "group")], vec![]),
      ],
    );
    let required_radio = &form_unchecked.children[0];
    let ancestors: Vec<&DomNode> = vec![&form_unchecked];
    assert!(matches(required_radio, &ancestors, &PseudoClass::Invalid));
    assert!(!matches(required_radio, &ancestors, &PseudoClass::Valid));
  }

  #[test]
  fn select_value_falls_back_to_first_option_when_all_disabled() {
    let select = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![
              ("disabled".to_string(), "disabled".to_string()),
              ("value".to_string(), "a".to_string()),
            ],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![
              ("disabled".to_string(), "disabled".to_string()),
              ("value".to_string(), "b".to_string()),
            ],
          },
          children: vec![],
        },
      ],
    };

    let value = ElementRef::new(&select).control_value();
    assert_eq!(value.as_deref(), Some("a"));
  }

  #[test]
  fn select_size_parsing_edge_cases() {
    let dropdown_size0 = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("size".to_string(), "0".to_string())],
      },
      children: vec![],
    };
    assert!(!select_is_listbox(&dropdown_size0));
    assert_eq!(select_effective_size(&dropdown_size0), 1);

    let dropdown_negative = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("size".to_string(), "-3".to_string())],
      },
      children: vec![],
    };
    assert!(!select_is_listbox(&dropdown_negative));
    assert_eq!(select_effective_size(&dropdown_negative), 1);

    let dropdown_invalid = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("size".to_string(), "abc".to_string())],
      },
      children: vec![],
    };
    assert!(!select_is_listbox(&dropdown_invalid));
    assert_eq!(select_effective_size(&dropdown_invalid), 1);

    let multi_default = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("multiple".to_string(), "multiple".to_string())],
      },
      children: vec![],
    };
    assert!(select_is_listbox(&multi_default));
    assert_eq!(select_effective_size(&multi_default), 4);

    let multi_invalid_size = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("multiple".to_string(), "multiple".to_string()),
          ("size".to_string(), "0".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(select_is_listbox(&multi_invalid_size));
    assert_eq!(select_effective_size(&multi_invalid_size), 4);

    let multi_size3 = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("multiple".to_string(), "multiple".to_string()),
          ("size".to_string(), "3".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(select_is_listbox(&multi_size3));
    assert_eq!(select_effective_size(&multi_size3), 3);
  }

  #[test]
  fn indeterminate_matches_checkbox_and_progress() {
    let checkbox = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "checkbox".to_string()),
          ("indeterminate".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&checkbox, &[], &PseudoClass::Indeterminate));

    let normal_checkbox = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("type".to_string(), "checkbox".to_string())],
      },
      children: vec![],
    };
    assert!(!matches(&normal_checkbox, &[], &PseudoClass::Indeterminate));

    let radio = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "radio".to_string()),
          ("indeterminate".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(!matches(&radio, &[], &PseudoClass::Indeterminate));

    let progress_indeterminate = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "progress".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };
    assert!(matches(
      &progress_indeterminate,
      &[],
      &PseudoClass::Indeterminate
    ));

    let progress_value = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "progress".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("value".to_string(), "0.5".to_string())],
      },
      children: vec![],
    };
    assert!(!matches(&progress_value, &[], &PseudoClass::Indeterminate));

    let progress_invalid_value = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "progress".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("value".to_string(), "not-a-number".to_string())],
      },
      children: vec![],
    };
    assert!(matches(
      &progress_invalid_value,
      &[],
      &PseudoClass::Indeterminate
    ));
  }

  #[test]
  fn default_matches_submit_controls_and_options() {
    let form = element(
      "form",
      vec![
        element("input", vec![]),
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "button".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "button".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("type".to_string(), "submit".to_string())],
          },
          children: vec![],
        },
      ],
    );
    let ancestors: Vec<&DomNode> = vec![&form];
    let default_button = &form.children[1];
    let submit_button = &form.children[2];
    assert!(matches(default_button, &ancestors, &PseudoClass::Default));
    assert!(!matches(submit_button, &ancestors, &PseudoClass::Default));

    let form_disabled_first = element(
      "form",
      vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "button".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("disabled".to_string(), "disabled".to_string())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "button".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
      ],
    );
    let ancestors: Vec<&DomNode> = vec![&form_disabled_first];
    let disabled = &form_disabled_first.children[0];
    let enabled = &form_disabled_first.children[1];
    assert!(!matches(disabled, &ancestors, &PseudoClass::Default));
    assert!(matches(enabled, &ancestors, &PseudoClass::Default));

    let select = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("disabled".to_string(), "disabled".to_string())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
      ],
    };
    let ancestors: Vec<&DomNode> = vec![&select];
    let first = &select.children[0];
    let second = &select.children[1];
    assert!(!matches(first, &ancestors, &PseudoClass::Default));
    assert!(matches(second, &ancestors, &PseudoClass::Default));

    let checkbox = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "checkbox".to_string()),
          ("checked".to_string(), "checked".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&checkbox, &[], &PseudoClass::Default));
  }

  #[test]
  fn disabled_and_enabled_match_controls() {
    let input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };
    assert!(matches(&input, &[], &PseudoClass::Enabled));
    assert!(!matches(&input, &[], &PseudoClass::Disabled));

    let disabled_button = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "button".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("disabled".to_string(), "true".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&disabled_button, &[], &PseudoClass::Disabled));
    assert!(!matches(&disabled_button, &[], &PseudoClass::Enabled));

    // Fieldset disables descendants except inside first legend
    let legend_child = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };
    let legend = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "legend".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![legend_child.clone()],
    };
    let outer_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };
    let fieldset = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "fieldset".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("disabled".to_string(), "true".to_string())],
      },
      children: vec![legend.clone(), outer_input.clone()],
    };

    let anc_outer: Vec<&DomNode> = vec![&fieldset];
    assert!(matches(&outer_input, &anc_outer, &PseudoClass::Disabled));
    assert!(!matches(&outer_input, &anc_outer, &PseudoClass::Enabled));

    let anc_legend: Vec<&DomNode> = vec![&fieldset, &fieldset.children[0]];
    let legend_child_ref = &fieldset.children[0].children[0];
    assert!(!matches(
      legend_child_ref,
      &anc_legend,
      &PseudoClass::Disabled
    ));
    assert!(matches(
      legend_child_ref,
      &anc_legend,
      &PseudoClass::Enabled
    ));
  }

  #[test]
  fn valid_invalid_and_range_match_controls() {
    let text_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("type".to_string(), "text".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&text_input, &[], &PseudoClass::Valid));
    assert!(
      !matches(&text_input, &[], &PseudoClass::UserValid),
      "user-validity is initially false"
    );
    assert!(!matches(&text_input, &[], &PseudoClass::Invalid));
    assert!(!matches(&text_input, &[], &PseudoClass::UserInvalid));

    let text_input_user_validity = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "text".to_string()),
          ("data-fastr-user-validity".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(
      &text_input_user_validity,
      &[],
      &PseudoClass::UserValid
    ));
    assert!(!matches(
      &text_input_user_validity,
      &[],
      &PseudoClass::UserInvalid
    ));

    let required_empty = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("required".to_string(), "true".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&required_empty, &[], &PseudoClass::Invalid));
    assert!(
      !matches(&required_empty, &[], &PseudoClass::UserInvalid),
      "user-invalid is gated by user validity"
    );
    assert!(!matches(&required_empty, &[], &PseudoClass::Valid));
    assert!(!matches(&required_empty, &[], &PseudoClass::UserValid));

    let required_empty_user_validity = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("data-fastr-user-validity".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(
      &required_empty_user_validity,
      &[],
      &PseudoClass::UserInvalid
    ));
    assert!(!matches(
      &required_empty_user_validity,
      &[],
      &PseudoClass::UserValid
    ));

    let number_in_range = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "number".to_string()),
          ("value".to_string(), "5".to_string()),
          ("min".to_string(), "1".to_string()),
          ("max".to_string(), "10".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&number_in_range, &[], &PseudoClass::Valid));
    assert!(!matches(&number_in_range, &[], &PseudoClass::UserValid));
    assert!(matches(&number_in_range, &[], &PseudoClass::InRange));
    assert!(!matches(&number_in_range, &[], &PseudoClass::OutOfRange));

    let number_out_of_range = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "number".to_string()),
          ("value".to_string(), "15".to_string()),
          ("min".to_string(), "1".to_string()),
          ("max".to_string(), "10".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&number_out_of_range, &[], &PseudoClass::Invalid));
    assert!(!matches(&number_out_of_range, &[], &PseudoClass::UserInvalid));
    assert!(matches(&number_out_of_range, &[], &PseudoClass::OutOfRange));
    assert!(!matches(&number_out_of_range, &[], &PseudoClass::InRange));

    let number_nan = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "number".to_string()),
          ("value".to_string(), "abc".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&number_nan, &[], &PseudoClass::Valid));
    assert!(
      !matches(&number_nan, &[], &PseudoClass::UserValid),
      "user-validity is initially false"
    );
    assert!(!matches(&number_nan, &[], &PseudoClass::Invalid));
    assert!(!matches(&number_nan, &[], &PseudoClass::UserInvalid));

    let date_invalid = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "date".to_string()),
          ("value".to_string(), "2020-13-01".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&date_invalid, &[], &PseudoClass::Valid));
    assert!(!matches(&date_invalid, &[], &PseudoClass::Invalid));

    let required_date_invalid = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "date".to_string()),
          ("value".to_string(), "2020-13-01".to_string()),
          ("required".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(
      &required_date_invalid,
      &[],
      &PseudoClass::Invalid
    ));
    assert!(!matches(
      &required_date_invalid,
      &[],
      &PseudoClass::Valid
    ));

    let disabled_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("disabled".to_string(), "true".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&disabled_input, &[], &PseudoClass::Valid));
    assert!(!matches(&disabled_input, &[], &PseudoClass::UserValid));
    assert!(!matches(&disabled_input, &[], &PseudoClass::Invalid));
    assert!(!matches(&disabled_input, &[], &PseudoClass::UserInvalid));

    let required_multiple_select = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("multiple".to_string(), "multiple".to_string()),
        ],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      }],
    };
    assert!(matches(
      &required_multiple_select,
      &[],
      &PseudoClass::Invalid
    ));

    let valid_multiple_select = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("multiple".to_string(), "multiple".to_string()),
        ],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![
            ("selected".to_string(), "selected".to_string()),
            ("value".to_string(), "a".to_string()),
          ],
        },
        children: vec![],
      }],
    };
    assert!(matches(&valid_multiple_select, &[], &PseudoClass::Valid));
    assert!(!matches(&valid_multiple_select, &[], &PseudoClass::Invalid));

    let valid_multiple_select_empty_value = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("multiple".to_string(), "multiple".to_string()),
        ],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![
            ("selected".to_string(), "selected".to_string()),
            ("value".to_string(), String::new()),
          ],
        },
        children: vec![],
      }],
    };
    assert!(matches(
      &valid_multiple_select_empty_value,
      &[],
      &PseudoClass::Valid
    ));
    assert!(!matches(
      &valid_multiple_select_empty_value,
      &[],
      &PseudoClass::Invalid
    ));

    let invalid_multiple_select_disabled_selected = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("multiple".to_string(), "multiple".to_string()),
        ],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![
            ("disabled".to_string(), "disabled".to_string()),
            ("selected".to_string(), "selected".to_string()),
            ("value".to_string(), "a".to_string()),
          ],
        },
        children: vec![],
      }],
    };
    assert!(matches(
      &invalid_multiple_select_disabled_selected,
      &[],
      &PseudoClass::Invalid
    ));
    assert!(!matches(
      &invalid_multiple_select_disabled_selected,
      &[],
      &PseudoClass::Valid
    ));

    let invalid_multiple_select_selected_in_disabled_optgroup = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("multiple".to_string(), "multiple".to_string()),
        ],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "optgroup".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![
            ("disabled".to_string(), "disabled".to_string()),
            ("label".to_string(), "g".to_string()),
          ],
        },
        children: vec![DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![
              ("selected".to_string(), "selected".to_string()),
              ("value".to_string(), "a".to_string()),
            ],
          },
          children: vec![],
        }],
      }],
    };
    assert!(matches(
      &invalid_multiple_select_selected_in_disabled_optgroup,
      &[],
      &PseudoClass::Invalid
    ));
    assert!(!matches(
      &invalid_multiple_select_selected_in_disabled_optgroup,
      &[],
      &PseudoClass::Valid
    ));

    let multiple_select_value = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("multiple".to_string(), "multiple".to_string())],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![
              ("selected".to_string(), "selected".to_string()),
              ("value".to_string(), "a".to_string()),
            ],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![
              ("selected".to_string(), "selected".to_string()),
              ("value".to_string(), "b".to_string()),
            ],
          },
          children: vec![],
        },
      ],
    };
    assert_eq!(
      ElementRef::new(&multiple_select_value)
        .control_value()
        .expect("expected select value"),
      "a"
    );

    let valid_required_single_select_empty_value = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("required".to_string(), "true".to_string())],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("value".to_string(), "a".to_string())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![
              ("selected".to_string(), "selected".to_string()),
              ("value".to_string(), String::new()),
            ],
          },
          children: vec![],
        },
      ],
    };
    assert!(matches(
      &valid_required_single_select_empty_value,
      &[],
      &PseudoClass::Valid
    ));
    assert!(!matches(
      &valid_required_single_select_empty_value,
      &[],
      &PseudoClass::Invalid
    ));

    let required_single_select_placeholder = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("required".to_string(), "true".to_string())],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("value".to_string(), String::new())],
          },
          children: vec![],
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "option".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![("value".to_string(), "a".to_string())],
          },
          children: vec![],
        },
      ],
    };
    assert!(matches(
      &required_single_select_placeholder,
      &[],
      &PseudoClass::Invalid
    ));
    assert!(!matches(
      &required_single_select_placeholder,
      &[],
      &PseudoClass::Valid
    ));

    let valid_required_size_select_empty_value = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("required".to_string(), "true".to_string()),
          ("size".to_string(), "2".to_string()),
        ],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("value".to_string(), String::new())],
        },
        children: vec![],
      }],
    };
    assert!(matches(
      &valid_required_size_select_empty_value,
      &[],
      &PseudoClass::Valid
    ));
    assert!(!matches(
      &valid_required_size_select_empty_value,
      &[],
      &PseudoClass::Invalid
    ));
  }

  #[test]
  fn select_required_validation_matches_html_semantics() {
    let missing_multiple = element_with_attrs(
      "select",
      vec![("required", ""), ("multiple", "")],
      vec![element("option", vec![])],
    );
    assert!(
      !ElementRef::new(&missing_multiple).accessibility_is_valid(),
      "<select multiple required> with no selected options is invalid"
    );

    let present_multiple_empty_value = element_with_attrs(
      "select",
      vec![("required", ""), ("multiple", "")],
      vec![element_with_attrs(
        "option",
        vec![("selected", ""), ("value", "")],
        vec![],
      )],
    );
    assert!(
      ElementRef::new(&present_multiple_empty_value).accessibility_is_valid(),
      "<select multiple required> is valid when any option is selected, even if its value is empty"
    );

    let disabled_selected_multiple = element_with_attrs(
      "select",
      vec![("required", ""), ("multiple", "")],
      vec![element_with_attrs(
        "option",
        vec![("selected", ""), ("disabled", ""), ("value", "")],
        vec![],
      )],
    );
    assert!(
      !ElementRef::new(&disabled_selected_multiple).accessibility_is_valid(),
      "<select multiple required> is invalid when the only selected option is disabled"
    );

    let disabled_placeholder_single = element_with_attrs(
      "select",
      vec![("required", "")],
      vec![
        element_with_attrs(
          "option",
          vec![("selected", ""), ("disabled", ""), ("value", "")],
          vec![text("Pick one")],
        ),
        element_with_attrs("option", vec![("value", "x")], vec![text("X")]),
      ],
    );
    assert!(
      !ElementRef::new(&disabled_placeholder_single).accessibility_is_valid(),
      "<select required> is invalid when the placeholder label option is the only selected option"
    );

    let optgroup_first_option_empty_value = element_with_attrs(
      "select",
      vec![("required", "")],
      vec![
        element_with_attrs(
          "optgroup",
          vec![("label", "g")],
          vec![element_with_attrs(
            "option",
            vec![("selected", ""), ("value", "")],
            vec![],
          )],
        ),
        element_with_attrs("option", vec![("value", "x")], vec![]),
      ],
    );
    assert!(
      ElementRef::new(&optgroup_first_option_empty_value).accessibility_is_valid(),
      "<select required> is valid when the first option is inside an <optgroup>, even if its value is empty"
    );

    let hidden_placeholder_single = element_with_attrs(
      "select",
      vec![("required", "")],
      vec![
        element_with_attrs("option", vec![("hidden", ""), ("value", "")], vec![]),
        element_with_attrs("option", vec![("value", "x")], vec![]),
      ],
    );
    assert!(
      ElementRef::new(&hidden_placeholder_single).accessibility_is_valid(),
      "<select required> should ignore hidden options when determining placeholder label option"
    );

    let hidden_selected_multiple = element_with_attrs(
      "select",
      vec![("required", ""), ("multiple", "")],
      vec![element_with_attrs(
        "option",
        vec![("hidden", ""), ("selected", ""), ("value", "x")],
        vec![],
      )],
    );
    assert!(
      !ElementRef::new(&hidden_selected_multiple).accessibility_is_valid(),
      "<select multiple required> is invalid when the only selected option is hidden"
    );

    let hidden_optgroup_selected_multiple = element_with_attrs(
      "select",
      vec![("required", ""), ("multiple", "")],
      vec![element_with_attrs(
        "optgroup",
        vec![("hidden", ""), ("label", "g")],
        vec![element_with_attrs("option", vec![("selected", ""), ("value", "x")], vec![])],
      )],
    );
    assert!(
      !ElementRef::new(&hidden_optgroup_selected_multiple).accessibility_is_valid(),
      "<select multiple required> is invalid when the only selected option is in a hidden optgroup"
    );

    let single_last_selected_wins = element_with_attrs(
      "select",
      vec![("required", "")],
      vec![
        element_with_attrs("option", vec![("selected", ""), ("value", "")], vec![]),
        element_with_attrs("option", vec![("selected", ""), ("value", "x")], vec![]),
      ],
    );
    assert!(
      ElementRef::new(&single_last_selected_wins).accessibility_is_valid(),
      "single selects should treat the last selected option as the effective selection"
    );
  }

  #[test]
  fn inline_style_display_uses_last_valid_declaration_for_select_hidden_heuristics() {
    let display_none_then_block = element_with_attrs(
      "option",
      vec![("style", "display:none; display:block")],
      vec![],
    );
    assert!(
      !node_hidden_for_select(&display_none_then_block),
      "later inline display declaration should override earlier ones"
    );

    let invalid_then_none = element_with_attrs(
      "option",
      vec![("style", "display:; display:none")],
      vec![],
    );
    assert!(
      node_hidden_for_select(&invalid_then_none),
      "invalid inline display declarations should not mask later valid ones"
    );
  }

  #[test]
  fn non_ascii_whitespace_inline_style_display_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let style = format!("display:{nbsp}none");
    let option = element_with_attrs("option", vec![("style", style.as_str())], vec![]);
    assert!(
      !node_hidden_for_select(&option),
      "NBSP must not be treated as whitespace when parsing inline style declarations"
    );
  }

  #[test]
  fn read_only_and_read_write_match_form_controls() {
    let text_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("type".to_string(), "text".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&text_input, &[], &PseudoClass::ReadWrite));
    assert!(!matches(&text_input, &[], &PseudoClass::ReadOnly));

    let readonly_input = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("type".to_string(), "text".to_string()),
          ("readonly".to_string(), "readonly".to_string()),
        ],
      },
      children: vec![],
    };
    assert!(matches(&readonly_input, &[], &PseudoClass::ReadOnly));
    assert!(!matches(&readonly_input, &[], &PseudoClass::ReadWrite));

    let disabled_textarea = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "textarea".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("disabled".to_string(), "true".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&disabled_textarea, &[], &PseudoClass::ReadOnly));
    assert!(!matches(&disabled_textarea, &[], &PseudoClass::ReadWrite));

    let checkbox = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "input".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("type".to_string(), "checkbox".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&checkbox, &[], &PseudoClass::ReadOnly));
    assert!(!matches(&checkbox, &[], &PseudoClass::ReadWrite));

    let select = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };
    assert!(matches(&select, &[], &PseudoClass::ReadWrite));
    assert!(!matches(&select, &[], &PseudoClass::ReadOnly));

    let editable_div = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("contenteditable".to_string(), "true".to_string())],
      },
      children: vec![],
    };
    assert!(matches(&editable_div, &[], &PseudoClass::ReadWrite));
    assert!(!matches(&editable_div, &[], &PseudoClass::ReadOnly));
  }

  #[test]
  fn target_matches_id_and_name() {
    let target = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("id".to_string(), "section".to_string())],
      },
      children: vec![],
    };
    with_target_fragment(Some("#section"), || {
      assert!(matches(&target, &[], &PseudoClass::Target));
    });
    with_target_fragment(Some("section"), || {
      assert!(matches(&target, &[], &PseudoClass::Target));
    });
    with_target_fragment(Some("other"), || {
      assert!(!matches(&target, &[], &PseudoClass::Target));
    });

    let unicode_target = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("id".to_string(), "café".to_string())],
      },
      children: vec![],
    };
    with_target_fragment(Some("#caf%C3%A9"), || {
      assert!(matches(&unicode_target, &[], &PseudoClass::Target));
    });

    let anchor = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "a".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("name".to_string(), "anchor".to_string())],
      },
      children: vec![],
    };
    with_target_fragment(Some("anchor"), || {
      assert!(matches(&anchor, &[], &PseudoClass::Target));
    });
  }

  #[test]
  fn target_within_matches_descendants() {
    let target = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("id".to_string(), "section".to_string())],
      },
      children: vec![],
    };
    let container = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "main".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![target],
    };
    let other = element("p", vec![]);
    let root = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "body".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![container, other],
    };

    let children = &root.children;
    let container_ref = children.first().unwrap();
    let target_ref = container_ref.children.first().unwrap();
    let other_ref = children.get(1).unwrap();

    with_target_fragment(Some("#section"), || {
      assert!(matches(&root, &[], &PseudoClass::TargetWithin));
      assert!(matches(
        &container_ref,
        &[&root],
        &PseudoClass::TargetWithin
      ));
      assert!(matches(
        target_ref,
        &[&root, container_ref],
        &PseudoClass::TargetWithin
      ));
      assert!(!matches(&other_ref, &[&root], &PseudoClass::TargetWithin));
    });
  }

  #[test]
  fn parse_html_preserves_text_content() {
    let html = "<!doctype html><html><body><div><h1>Example Domain</h1><p>This domain is for use in documentation examples without needing permission.</p></div></body></html>";
    let dom = parse_html(html).expect("parse");
    fn contains_text(node: &DomNode, needle: &str) -> bool {
      match &node.node_type {
        DomNodeType::Text { content } => content.contains(needle),
        _ => node.children.iter().any(|c| contains_text(c, needle)),
      }
    }
    assert!(contains_text(&dom, "Example Domain"));
    assert!(contains_text(&dom, "documentation examples"));
  }

  #[test]
  fn parse_html_with_scripting_disabled_parses_noscript_children_as_dom() {
    let html =
      "<!doctype html><html><body><noscript id='ns'><p>fallback</p></noscript></body></html>";
    let dom =
      parse_html_with_options(html, DomParseOptions::with_scripting_enabled(false)).expect("parse");
    let noscript = find_element_by_id(&dom, "ns").expect("noscript element");
    assert!(
      noscript.children.iter().any(|child| {
        matches!(&child.node_type, DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("p"))
      }),
      "<noscript> should parse its contents as normal DOM when scripting is disabled"
    );
  }

  #[test]
  fn parse_html_with_scripting_enabled_parses_noscript_children_as_text() {
    let html =
      "<!doctype html><html><body><noscript id='ns'><p>fallback</p></noscript></body></html>";
    let dom =
      parse_html_with_options(html, DomParseOptions::with_scripting_enabled(true)).expect("parse");
    let noscript = find_element_by_id(&dom, "ns").expect("noscript element");
    assert_eq!(
      noscript.children.len(),
      1,
      "<noscript> should have a single text child when scripting is enabled"
    );
    match &noscript.children[0].node_type {
      DomNodeType::Text { content } => {
        assert!(
          content.contains("<p>fallback</p>"),
          "noscript text should contain raw HTML: {content:?}"
        );
      }
      other => panic!("expected noscript child to be text, got {other:?}"),
    }
  }

  #[test]
  fn parse_html_keeps_noscript_content_without_scripting() {
    let html = "<!doctype html><html><head><noscript><style>.fallback{color:red;}</style></noscript></head><body><noscript><div id='fallback'>hello</div></noscript></body></html>";
    let dom = parse_html(html).expect("parse");

    let fallback = find_element_by_id(&dom, "fallback").expect("noscript content parsed into DOM");
    let has_text_child = fallback.children.iter().any(|child| {
      if let DomNodeType::Text { content } = &child.node_type {
        content.contains("hello")
      } else {
        false
      }
    });
    assert!(
      has_text_child,
      "noscript children should be parsed as normal content"
    );
  }

  #[test]
  fn parse_html_preserves_head_noscript_children() {
    let html = "<!doctype html><html><head><noscript><style id='fallback-style'>body{color:green;}</style></noscript></head><body></body></html>";
    let dom = parse_html(html).expect("parse");

    let style = find_element_by_id(&dom, "fallback-style");
    assert!(
      style.is_some(),
      "style inside <noscript> in <head> should be retained"
    );
  }

  #[test]
  fn parse_html_ignores_noscript_content_with_scripting_enabled() {
    let html = "<!doctype html><html><head><noscript><style id='fallback-style'>.fallback{color:red;}</style></noscript></head><body><noscript><div id='fallback'>hello</div></noscript></body></html>";
    let dom = parse_html_with_options(html, DomParseOptions::javascript_enabled()).expect("parse");

    assert!(
      find_element_by_id(&dom, "fallback-style").is_none(),
      "head <noscript> children should not be parsed when scripting is enabled"
    );
    assert!(
      find_element_by_id(&dom, "fallback").is_none(),
      "body <noscript> children should not be parsed when scripting is enabled"
    );
  }

  #[test]
  fn scope_matches_document_root_only() {
    let child = element("div", vec![]);
    let root = element("html", vec![child.clone()]);
    let ancestors: Vec<&DomNode> = vec![&root];
    assert!(matches(&root, &[], &PseudoClass::Scope));
    assert!(!matches(&child, &ancestors, &PseudoClass::Scope));
  }

  #[test]
  fn pseudo_element_matching_reports_supported_pseudos() {
    let node = element("div", vec![]);
    let ancestors: Vec<&DomNode> = vec![];
    let mut caches = SelectorCaches::default();
    let cache_epoch = next_selector_cache_epoch();
    caches.set_epoch(cache_epoch);
    let sibling_cache = SiblingListCache::new(cache_epoch);
    let mut context = MatchingContext::new(
      MatchingMode::ForStatelessPseudoElement,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document().with_sibling_cache(&sibling_cache);
    let element_ref = ElementRef::with_ancestors(&node, &ancestors);

    assert!(element_ref.match_pseudo_element(&PseudoElement::Before, &mut context));
    assert!(element_ref.match_pseudo_element(&PseudoElement::After, &mut context));
    assert!(element_ref.match_pseudo_element(&PseudoElement::Marker, &mut context));
  }

  #[test]
  fn pseudo_element_matching_gates_form_controls() {
    let mut caches = SelectorCaches::default();
    let cache_epoch = next_selector_cache_epoch();
    caches.set_epoch(cache_epoch);
    let sibling_cache = SiblingListCache::new(cache_epoch);
    let mut context = MatchingContext::new(
      MatchingMode::ForStatelessPseudoElement,
      None,
      &mut caches,
      QuirksMode::NoQuirks,
      NeedsSelectorFlags::No,
      MatchingForInvalidation::No,
    );
    context.extra_data = ShadowMatchData::for_document().with_sibling_cache(&sibling_cache);

    let input_text = element_with_attrs("input", vec![("type", "text")], vec![]);
    let input_text_ref = ElementRef::new(&input_text);
    assert!(!input_text_ref.match_pseudo_element(&PseudoElement::SliderThumb, &mut context));
    assert!(
      !input_text_ref.match_pseudo_element(&PseudoElement::FileSelectorButton, &mut context),
      "file-selector-button should not match non-file inputs"
    );

    let input_range = element_with_attrs("input", vec![("type", "range")], vec![]);
    let input_range_ref = ElementRef::new(&input_range);
    assert!(input_range_ref.match_pseudo_element(&PseudoElement::SliderThumb, &mut context));

    let input_file = element_with_attrs("input", vec![("type", "file")], vec![]);
    let input_file_ref = ElementRef::new(&input_file);
    assert!(
      input_file_ref.match_pseudo_element(&PseudoElement::FileSelectorButton, &mut context),
      "file-selector-button should match file inputs"
    );

    let placeholder_empty = element_with_attrs(
      "input",
      vec![("type", "text"), ("placeholder", "x"), ("value", "")],
      vec![],
    );
    let placeholder_empty_ref = ElementRef::new(&placeholder_empty);
    assert!(placeholder_empty_ref.match_pseudo_element(&PseudoElement::Placeholder, &mut context));

    let placeholder_filled = element_with_attrs(
      "input",
      vec![("type", "text"), ("placeholder", "x"), ("value", "hello")],
      vec![],
    );
    let placeholder_filled_ref = ElementRef::new(&placeholder_filled);
    assert!(!placeholder_filled_ref.match_pseudo_element(&PseudoElement::Placeholder, &mut context));
  }

  #[test]
  fn parse_html_leaves_classes_untouched_by_default() {
    let dom = parse_html("<html class='no-js foo'><body></body></html>").expect("parse");
    let html = dom
      .children
      .iter()
      .find(|c| matches!(c.node_type, DomNodeType::Element { .. }))
      .expect("html child");
    let classes = match &html.node_type {
      DomNodeType::Element { attributes, .. } => attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("class"))
        .map(|(_, v)| v.split_ascii_whitespace().collect::<Vec<_>>())
        .unwrap_or_default(),
      _ => panic!("expected html element"),
    };
    assert!(classes.contains(&"no-js"));
    assert!(!classes.contains(&"js-enabled"));
    assert!(!classes.contains(&"jsl10n-visible"));

    let body = html
      .children
      .iter()
      .find(|c| {
        if let DomNodeType::Element { tag_name, .. } = &c.node_type {
          tag_name.eq_ignore_ascii_case("body")
        } else {
          false
        }
      })
      .expect("body child");
    let body_classes = match &body.node_type {
      DomNodeType::Element { attributes, .. } => attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("class"))
        .map(|(_, v)| v.split_ascii_whitespace().collect::<Vec<_>>())
        .unwrap_or_default(),
      _ => panic!("expected body element"),
    };
    assert!(!body_classes.contains(&"jsl10n-visible"));
  }

  #[test]
  fn non_ascii_whitespace_class_attribute_does_not_split_nbsp() {
    fn find_div<'a>(node: &'a DomNode) -> Option<&'a DomNode> {
      if node
        .tag_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("div"))
      {
        return Some(node);
      }
      node.children.iter().find_map(find_div)
    }

    let nbsp = "\u{00A0}";
    let markup = format!("<html><body><div class='foo{nbsp}bar'></div></body></html>");
    let dom = parse_html(&markup).expect("parse");
    let div = find_div(&dom).expect("div node");
    assert!(!div.has_class("foo"));
    assert!(!div.has_class("bar"));
    assert!(div.has_class(&format!("foo{nbsp}bar")));
  }

  #[test]
  fn parse_html_compat_mode_flips_no_js_class() {
    let dom = parse_html_with_options(
      "<html class='no-js foo'><body></body></html>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let html = dom
      .children
      .iter()
      .find(|c| matches!(c.node_type, DomNodeType::Element { .. }))
      .expect("html child");
    let classes = match &html.node_type {
      DomNodeType::Element { attributes, .. } => attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("class"))
        .map(|(_, v)| v.split_ascii_whitespace().collect::<Vec<_>>())
        .unwrap_or_default(),
      _ => panic!("expected html element"),
    };
    assert!(!classes.contains(&"no-js"));
    assert!(classes.contains(&"js-enabled"));
    assert!(classes.contains(&"foo"));
    assert!(classes.contains(&"jsl10n-visible"));
  }

  #[test]
  fn parse_html_compat_mode_adds_jsl10n_visible_when_missing() {
    let dom = parse_html_with_options(
      "<html><body></body></html>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let html = dom
      .children
      .iter()
      .find(|c| matches!(c.node_type, DomNodeType::Element { .. }))
      .expect("html child");
    let classes = match &html.node_type {
      DomNodeType::Element { attributes, .. } => attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("class"))
        .map(|(_, v)| v.split_ascii_whitespace().collect::<Vec<_>>())
        .unwrap_or_default(),
      _ => panic!("expected html element"),
    };
    assert!(classes.contains(&"jsl10n-visible"));
  }

  #[test]
  fn parse_html_compat_mode_marks_body_jsl10n_visible() {
    let dom = parse_html_with_options(
      "<html><body class='portal'></body></html>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let html = dom
      .children
      .iter()
      .find(|c| matches!(c.node_type, DomNodeType::Element { .. }))
      .expect("html child");
    let body = html
      .children
      .iter()
      .find(|c| {
        if let DomNodeType::Element { tag_name, .. } = &c.node_type {
          tag_name.eq_ignore_ascii_case("body")
        } else {
          false
        }
      })
      .expect("body child");

    let classes = match &body.node_type {
      DomNodeType::Element { attributes, .. } => attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("class"))
        .map(|(_, v)| v.split_ascii_whitespace().collect::<Vec<_>>())
        .unwrap_or_default(),
      _ => panic!("expected body element"),
    };
    assert!(classes.contains(&"portal"));
    assert!(classes.contains(&"jsl10n-visible"));
  }

  #[test]
  fn parse_html_compat_mode_copies_data_gl_src_into_img_attrs() {
    let dom = parse_html_with_options(
      "<html><body><img data-gl-src='a.jpg' data-gl-srcset='a1.jpg 1x, a2.jpg 2x'></body></html>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let html = dom
      .children
      .iter()
      .find(|c| matches!(c.node_type, DomNodeType::Element { .. }))
      .expect("html child");
    let body = html
      .children
      .iter()
      .find(|c| matches!(&c.node_type, DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("body")))
      .expect("body child");
    let img = body
      .children
      .iter()
      .find(|c| matches!(&c.node_type, DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("img")))
      .expect("img child");
    let attrs = match &img.node_type {
      DomNodeType::Element { attributes, .. } => attributes,
      _ => panic!("expected img element"),
    };
    let src = attrs
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case("src"))
      .map(|(_, v)| v.as_str());
    let srcset = attrs
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case("srcset"))
      .map(|(_, v)| v.as_str());
    assert_eq!(src, Some("a.jpg"));
    assert_eq!(srcset, Some("a1.jpg 1x, a2.jpg 2x"));
  }

  #[test]
  fn parse_html_compat_mode_copies_data_src_into_img_src() {
    let dom = parse_html_with_options(
      "<img id='img' data-src='a.jpg'>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let img = find_element_by_id(&dom, "img").expect("img element");
    assert_eq!(img.get_attribute_ref("src"), Some("a.jpg"));
  }

  #[test]
  fn parse_html_compat_mode_copies_data_srcset_into_img_attrs() {
    let dom = parse_html_with_options(
      "<img id='img' data-srcset='a1.jpg 1x, a2.jpg 2x'>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let img = find_element_by_id(&dom, "img").expect("img element");
    assert_eq!(
      img.get_attribute_ref("srcset"),
      Some("a1.jpg 1x, a2.jpg 2x")
    );
  }

  #[test]
  fn parse_html_compat_mode_does_not_treat_nbsp_srcset_as_empty() {
    let nbsp = "\u{00A0}";
    let html = format!("<img id='img' srcset='{nbsp}' data-srcset='a1.jpg 1x'>");
    let dom = parse_html_with_options(&html, DomParseOptions::compatibility()).expect("parse");
    let img = find_element_by_id(&dom, "img").expect("img element");
    assert_eq!(img.get_attribute_ref("srcset"), Some(nbsp));
  }

  #[test]
  fn parse_html_compat_mode_replaces_placeholder_img_src() {
    let dom = parse_html_with_options(
      "<img id='img' src='data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==' data-src='real.jpg'>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let img = find_element_by_id(&dom, "img").expect("img element");
    assert_eq!(img.get_attribute_ref("src"), Some("real.jpg"));
  }

  #[test]
  fn parse_html_compat_mode_does_not_override_authored_img_src() {
    let dom = parse_html_with_options(
      "<img id='img' src='author.jpg' data-src='lazy.jpg'>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let img = find_element_by_id(&dom, "img").expect("img element");
    assert_eq!(img.get_attribute_ref("src"), Some("author.jpg"));
  }

  #[test]
  fn parse_html_compat_mode_copies_source_data_srcset_into_source_attrs() {
    let dom = parse_html_with_options(
      "<picture><source id='source' data-srcset='a.webp 1x, b.webp 2x'><img src='fallback.jpg'></picture>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let source = find_element_by_id(&dom, "source").expect("source element");
    assert_eq!(
      source.get_attribute_ref("srcset"),
      Some("a.webp 1x, b.webp 2x")
    );
  }

  #[test]
  fn parse_html_compat_mode_copies_data_video_urls_to_video_src() {
    let dom = parse_html_with_options(
      "<video id='video' data-video-urls='video.webm, video.mp4'></video>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let video = find_element_by_id(&dom, "video").expect("video element");
    assert_eq!(video.get_attribute_ref("src"), Some("video.mp4"));
  }

  #[test]
  fn parse_html_compat_mode_copies_data_poster_url_to_video_poster() {
    let dom = parse_html_with_options(
      "<video id='video' data-poster-url='poster.jpg'></video>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let video = find_element_by_id(&dom, "video").expect("video element");
    assert_eq!(video.get_attribute_ref("poster"), Some("poster.jpg"));
  }

  #[test]
  fn parse_html_compat_mode_propagates_wrapper_data_video_urls_into_child_video() {
    let dom = parse_html_with_options(
      "<div data-video-urls='bg.webm, bg.mp4' data-poster-url='bg.jpg'><video id='video'></video></div>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let video = find_element_by_id(&dom, "video").expect("video element");
    assert_eq!(video.get_attribute_ref("src"), Some("bg.mp4"));
    assert_eq!(video.get_attribute_ref("poster"), Some("bg.jpg"));
  }

  #[test]
  fn parse_html_compat_mode_does_not_override_authored_video_src_or_poster() {
    let dom = parse_html_with_options(
      "<video id='video' src='author.mp4' poster='author.jpg' data-video-urls='lazy.mp4' data-poster-url='lazy.jpg'></video>",
      DomParseOptions::compatibility(),
    )
    .expect("parse");
    let video = find_element_by_id(&dom, "video").expect("video element");
    assert_eq!(video.get_attribute_ref("src"), Some("author.mp4"));
    assert_eq!(video.get_attribute_ref("poster"), Some("author.jpg"));
  }

  #[test]
  fn parse_html_standard_keeps_data_gl_src_out_of_img_attrs() {
    let dom = parse_html(
      "<html><body><img data-gl-src='a.jpg' data-gl-srcset='a1.jpg 1x, a2.jpg 2x'></body></html>",
    )
    .expect("parse");
    let html = dom
      .children
      .iter()
      .find(|c| matches!(c.node_type, DomNodeType::Element { .. }))
      .expect("html child");
    let body = html
      .children
      .iter()
      .find(|c| matches!(&c.node_type, DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("body")))
      .expect("body child");
    let img = body
      .children
      .iter()
      .find(|c| matches!(&c.node_type, DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("img")))
      .expect("img child");
    assert!(
      img.get_attribute_ref("src").is_none(),
      "standard DOM parse should not synthesize src"
    );
    assert!(
      img.get_attribute_ref("srcset").is_none(),
      "standard DOM parse should not synthesize srcset"
    );
  }

  #[test]
  fn parse_html_standard_keeps_data_src_out_of_img_attrs() {
    let dom = parse_html("<img id='img' data-src='a.jpg'>").expect("parse");
    let img = find_element_by_id(&dom, "img").expect("img element");
    assert!(
      img.get_attribute_ref("src").is_none(),
      "standard DOM parse should not synthesize src"
    );
  }

  #[test]
  fn top_layer_state_sets_open_for_dialog_and_popover() {
    let mut dom = document(vec![
      element_with_attrs(
        "DIALOG",
        vec![("id", "dialog-open"), ("data-fastr-open", "TRUE")],
        vec![],
      ),
      element_with_attrs(
        "dialog",
        vec![
          ("id", "dialog-closed"),
          ("open", ""),
          ("data-fastr-open", "false"),
        ],
        vec![],
      ),
      element_with_attrs(
        "div",
        vec![
          ("id", "popover-open"),
          ("popover", ""),
          ("data-fastr-open", "oPeN"),
        ],
        vec![],
      ),
      element_with_attrs(
        "div",
        vec![
          ("id", "popover-closed"),
          ("popover", ""),
          ("open", ""),
          ("data-fastr-open", "FALSE"),
        ],
        vec![],
      ),
    ]);

    apply_top_layer_state_with_deadline(&mut dom).expect("apply top-layer state");

    let dialog_open = find_element_by_id(&dom, "dialog-open").expect("dialog-open");
    assert!(
      dialog_open.get_attribute_ref("open").is_some(),
      "data-fastr-open should force dialog open"
    );

    let dialog_closed = find_element_by_id(&dom, "dialog-closed").expect("dialog-closed");
    assert!(
      dialog_closed.get_attribute_ref("open").is_none(),
      "data-fastr-open=false should remove dialog open"
    );

    let popover_open = find_element_by_id(&dom, "popover-open").expect("popover-open");
    assert!(
      popover_open.get_attribute_ref("open").is_some(),
      "data-fastr-open should force popover open"
    );
    assert!(
      popover_open.get_attribute_ref("data-fastr-inert").is_none(),
      "non-modal top-layer content should not inert the document"
    );

    let popover_closed = find_element_by_id(&dom, "popover-closed").expect("popover-closed");
    assert!(
      popover_closed.get_attribute_ref("open").is_none(),
      "data-fastr-open=false should remove popover open"
    );
  }

  #[test]
  fn top_layer_state_inerts_outside_modal_dialog() {
    let mut dom = document(vec![
      element_with_attrs(
        "div",
        vec![("id", "outside")],
        vec![element_with_attrs(
          "span",
          vec![("id", "outside-child")],
          vec![],
        )],
      ),
      element_with_attrs(
        "dialog",
        vec![("id", "modal"), ("data-fastr-open", "modal")],
        vec![element_with_attrs("p", vec![("id", "inside")], vec![])],
      ),
      element_with_attrs(
        "div",
        vec![
          ("id", "popover"),
          ("popover", ""),
          ("data-fastr-open", "true"),
        ],
        vec![],
      ),
    ]);

    apply_top_layer_state_with_deadline(&mut dom).expect("apply top-layer state");

    let outside = find_element_by_id(&dom, "outside").expect("outside");
    assert_eq!(
      outside.get_attribute_ref("data-fastr-inert"),
      Some("true"),
      "outside subtree should be inert when a modal dialog is open"
    );
    let outside_child = find_element_by_id(&dom, "outside-child").expect("outside-child");
    assert_eq!(
      outside_child.get_attribute_ref("data-fastr-inert"),
      Some("true"),
      "descendants outside the modal subtree should also be inert"
    );

    let modal = find_element_by_id(&dom, "modal").expect("modal");
    assert!(
      modal.get_attribute_ref("open").is_some(),
      "modal dialog should be forced open"
    );
    assert!(
      modal.get_attribute_ref("data-fastr-inert").is_none(),
      "modal subtree should not be inert"
    );

    let inside = find_element_by_id(&dom, "inside").expect("inside");
    assert!(
      inside.get_attribute_ref("data-fastr-inert").is_none(),
      "modal descendants should not be inert"
    );

    let popover = find_element_by_id(&dom, "popover").expect("popover");
    assert!(
      popover.get_attribute_ref("open").is_some(),
      "popover open state should still be applied"
    );
    assert_eq!(
      popover.get_attribute_ref("data-fastr-inert"),
      Some("true"),
      "popover outside modal subtree should be inert"
    );
  }

  #[test]
  fn top_layer_state_does_not_inert_for_non_modal_dialogs() {
    let mut dom = document(vec![
      element_with_attrs("div", vec![("id", "outside")], vec![]),
      element_with_attrs(
        "dialog",
        vec![("id", "dialog"), ("open", "")],
        vec![element_with_attrs("p", vec![("id", "inside")], vec![])],
      ),
      element_with_attrs("div", vec![("id", "outside2")], vec![]),
    ]);

    apply_top_layer_state_with_deadline(&mut dom).expect("apply top-layer state");

    let outside = find_element_by_id(&dom, "outside").expect("outside");
    let outside2 = find_element_by_id(&dom, "outside2").expect("outside2");
    assert!(outside.get_attribute_ref("data-fastr-inert").is_none());
    assert!(outside2.get_attribute_ref("data-fastr-inert").is_none());

    let dialog = find_element_by_id(&dom, "dialog").expect("dialog");
    assert!(
      dialog.get_attribute_ref("open").is_some(),
      "open dialogs should remain open"
    );
  }

  #[test]
  fn top_layer_state_ignores_modal_dialogs_inside_templates() {
    let mut dom = document(vec![
      element_with_attrs("div", vec![("id", "outside")], vec![]),
      element_with_attrs(
        "template",
        vec![("id", "tpl")],
        vec![element_with_attrs(
          "dialog",
          vec![("id", "modal"), ("data-fastr-open", "modal")],
          vec![],
        )],
      ),
    ]);

    apply_top_layer_state_with_deadline(&mut dom).expect("apply top-layer state");

    let outside = find_element_by_id(&dom, "outside").expect("outside");
    assert!(
      outside.get_attribute_ref("data-fastr-inert").is_none(),
      "modal dialogs inside inert <template> contents must not inert the document"
    );
  }
}
