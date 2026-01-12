//! Public API integration tests.
//!
//! These tests exercise `fastrender` as an external consumer would (via `FastRender`), without
//! reaching into internal modules.
mod clip_mask_diagnostics;
mod csp_img_data_url;
mod css_double_fetch;
mod css_empty_url_no_fetch;
mod css_import_referrer_semantics;
mod dom2_geometry_scrollport;
mod js_exports;
mod quirks_body_percent_height;
mod prepared_document_geometry_tree;
mod root_font_size_percent;
mod svg_document_css;
mod svg_mask_image;
mod render_control;
