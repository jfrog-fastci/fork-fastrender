//! Audit (and optionally clean) the on-disk subresource cache directory.
//!
//! Pageset tooling relies on `fetches/assets/` for warm-cache runs. When bot mitigation or
//! transient failures inject HTML/403 responses into the disk cache, renders can lose
//! CSS/fonts/images while still appearing "warm".
//!
//! This binary provides deterministic diagnostics and opt-in cleanup for poisoned entries.

use clap::Parser;
use fastrender::cli_utils::args::DEFAULT_DISK_CACHE_LOCK_STALE_SECS;
use fastrender::cli_utils::disk_cache_audit::{audit_disk_cache_dir, DiskCacheAuditOptions, UrlCount};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_CACHE_DIR: &str = "fetches/assets";

#[derive(Debug, Parser)]
#[command(
  name = "disk_cache_audit",
  about = "Audit (and optionally clean) FastRender's disk-backed subresource cache"
)]
struct Cli {
  /// Disk cache directory to scan (flat; no recursion).
  #[arg(long, default_value = DEFAULT_CACHE_DIR, value_name = "PATH")]
  cache_dir: PathBuf,

  /// Emit a single JSON object (stable keys) for scripting.
  #[arg(long)]
  json: bool,

  /// Include the top N URLs per failure category (0 disables).
  #[arg(long, default_value_t = 10, value_name = "N")]
  top: usize,

  /// Delete entries whose cached metadata has an HTTP status code >= 400.
  #[arg(long)]
  delete_http_errors: bool,

  /// Delete entries that cached HTML but look like static subresources by URL extension.
  #[arg(long)]
  delete_html_subresources: bool,

  /// Delete entries that persist a fetch error (`error` field set).
  #[arg(long)]
  delete_error_entries: bool,

  /// Delete stale `.lock` files (older than `--lock-stale-after-secs`).
  #[arg(long)]
  delete_stale_locks: bool,

  /// Delete leftover `.tmp` files from partial cache writes.
  #[arg(long)]
  delete_tmp_files: bool,

  /// Maximum age in seconds for `.lock` files before they are treated as stale.
  ///
  /// Defaults to `FASTR_DISK_CACHE_LOCK_STALE_SECS` (when set) or 8 seconds.
  #[arg(
    long,
    env = "FASTR_DISK_CACHE_LOCK_STALE_SECS",
    default_value_t = DEFAULT_DISK_CACHE_LOCK_STALE_SECS,
    value_parser = clap::value_parser!(u64).range(1..),
    value_name = "SECS"
  )]
  lock_stale_after_secs: u64,
}

fn main() -> std::io::Result<()> {
  let cli = Cli::parse();
  let options = DiskCacheAuditOptions {
    delete_http_errors: cli.delete_http_errors,
    delete_html_subresources: cli.delete_html_subresources,
    delete_error_entries: cli.delete_error_entries,
    delete_stale_locks: cli.delete_stale_locks,
    delete_tmp_files: cli.delete_tmp_files,
    lock_stale_after: Duration::from_secs(cli.lock_stale_after_secs),
    top_n: cli.top,
  };
  let report = audit_disk_cache_dir(&cli.cache_dir, &options)?;

  if cli.json {
    let mut out = serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(ref mut obj) = out {
      obj.insert(
        "cache_dir".to_string(),
        serde_json::Value::String(cli.cache_dir.display().to_string()),
      );
    }
    println!(
      "{}",
      serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    );
    return Ok(());
  }

  println!("Disk cache audit: {}", cli.cache_dir.display());
  println!(
    "Entries: scanned={} parsed={} invalid_meta={}",
    report.entries_scanned, report.entries_parsed, report.invalid_meta_count
  );
  println!(
    "Files: bin={} bin_bytes={} meta={} alias={} locks={} stale_locks={} tmp={} journal_bytes={}",
    report.bin_count,
    report.bin_bytes,
    report.meta_count,
    report.alias_count,
    report.lock_count,
    report.stale_lock_count,
    report.tmp_count,
    report.journal_bytes
  );
  println!("HTTP errors (status>=400): {}", report.http_error_count);
  println!(
    "HTML masquerading as static subresources: {}",
    report.html_subresource_count
  );
  println!(
    "Persisted network errors (`error` field): {}",
    report.error_field_count
  );
  println!(
    "Stale locks: {} (threshold={}s)",
    report.stale_lock_count, cli.lock_stale_after_secs
  );
  println!("Tmp files: {}", report.tmp_count);

  if cli.delete_http_errors
    || cli.delete_html_subresources
    || cli.delete_error_entries
    || cli.delete_stale_locks
    || cli.delete_tmp_files
  {
    println!();
    println!(
      "Deleted: entries={} http_error_entries={} html_subresource_entries={} error_entries={} stale_lock_files={} tmp_files={} (bin={} meta={} alias={})",
      report.deleted_entry_count,
      report.deleted_http_error_entries,
      report.deleted_html_subresource_entries,
      report.deleted_error_entries,
      report.deleted_stale_lock_files,
      report.deleted_tmp_files,
      report.deleted_bin_files,
      report.deleted_meta_files,
      report.deleted_alias_files
    );
  }

  fn print_top(label: &str, items: &[UrlCount]) {
    if items.is_empty() {
      return;
    }
    println!();
    println!("{label}:");
    for item in items {
      println!("  [{}] {}", item.count, item.url);
    }
  }

  print_top("Top HTTP error URLs", &report.top_http_error_urls);
  print_top(
    "Top HTML-subresource URLs",
    &report.top_html_subresource_urls,
  );
  print_top("Top persisted-error URLs", &report.top_error_urls);

  Ok(())
}
