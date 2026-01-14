//! Shared `__fastrender_*` internal property keys.
//!
//! These strings are used by both the handwritten `vm-js` shims (`window_realm`) and the WebIDL
//! dispatch/runtime layers. Keeping them in one place prevents drift between backends (e.g. wrapper
//! caching and prototype lookups).

// --- DOM wrapper identity / caching ---

pub(crate) const NODE_ID_KEY: &str = "__fastrender_node_id";
pub(crate) const WRAPPER_DOCUMENT_KEY: &str = "__fastrender_wrapper_document";

// --- Live collections / wrapper-owned caches ---

pub(crate) const NODE_CHILD_NODES_KEY: &str = "__fastrender_node_child_nodes";
pub(crate) const NODE_CHILDREN_KEY: &str = "__fastrender_node_children";
pub(crate) const COLLECTION_LENGTH_KEY: &str = "__fastrender_collection_length";

pub(crate) const NODE_LIST_PROTOTYPE_KEY: &str = "__fastrender_node_list_prototype";
pub(crate) const HTML_COLLECTION_PROTOTYPE_KEY: &str = "__fastrender_html_collection_prototype";
pub(crate) const HTML_COLLECTION_ROOT_KEY: &str = "__fastrender_html_collection_root";
pub(crate) const NODE_ITERATOR_PROTOTYPE_KEY: &str = "__fastrender_node_iterator_prototype";

// --- Event wrappers ---

pub(crate) const EVENT_BRAND_KEY: &str = "__fastrender_event";
pub(crate) const EVENT_KIND_KEY: &str = "__fastrender_event_kind";
pub(crate) const EVENT_INITIALIZED_KEY: &str = "__fastrender_event_initialized";
pub(crate) const EVENT_IMMEDIATE_STOP_KEY: &str = "__fastrender_event_stop_immediate";
pub(crate) const EVENT_ID_KEY: &str = "__fastrender_event_id";

// --- CSSStyleDeclaration shims (Element.style) ---

pub(crate) const CSS_STYLE_DECL_PROTOTYPE_KEY: &str = "__fastrender_css_style_declaration_prototype";

pub(crate) const STYLE_GET_PROPERTY_VALUE_KEY: &str = "__fastrender_style_get_property_value";
pub(crate) const STYLE_SET_PROPERTY_KEY: &str = "__fastrender_style_set_property";
pub(crate) const STYLE_REMOVE_PROPERTY_KEY: &str = "__fastrender_style_remove_property";

pub(crate) const STYLE_CSS_TEXT_GET_KEY: &str = "__fastrender_style_css_text_get";
pub(crate) const STYLE_CSS_TEXT_SET_KEY: &str = "__fastrender_style_css_text_set";
pub(crate) const STYLE_DISPLAY_GET_KEY: &str = "__fastrender_style_display_get";
pub(crate) const STYLE_DISPLAY_SET_KEY: &str = "__fastrender_style_display_set";
pub(crate) const STYLE_CURSOR_GET_KEY: &str = "__fastrender_style_cursor_get";
pub(crate) const STYLE_CURSOR_SET_KEY: &str = "__fastrender_style_cursor_set";
pub(crate) const STYLE_HEIGHT_GET_KEY: &str = "__fastrender_style_height_get";
pub(crate) const STYLE_HEIGHT_SET_KEY: &str = "__fastrender_style_height_set";
pub(crate) const STYLE_WIDTH_GET_KEY: &str = "__fastrender_style_width_get";
pub(crate) const STYLE_WIDTH_SET_KEY: &str = "__fastrender_style_width_set";

// --- <iframe> / nested browsing context shims ---
//
// These keys store the minimal same-origin iframe state (Window-like object + Document wrapper)
// on the iframe element wrapper itself.
pub(crate) const IFRAME_CONTENT_DOCUMENT_KEY: &str = "__fastrender_iframe_content_document";
pub(crate) const IFRAME_CONTENT_WINDOW_KEY: &str = "__fastrender_iframe_content_window";
