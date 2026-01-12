//! Public API integration tests.
//!
//! These tests exercise `fastrender` as an external consumer would (via `FastRender`), without
//! reaching into internal modules.
mod cascade_diagnostics_env;
mod clip_mask_diagnostics;
mod csp_img_data_url;
mod css_double_fetch;
mod css_empty_url_no_fetch;
mod css_import_referrer_semantics;
mod js_exports;
mod prepared_document_geometry_tree;
mod root_font_size_percent;
mod svg_document_css;
mod svg_mask_image;
mod quirks_body_percent_height;
mod render_control;
