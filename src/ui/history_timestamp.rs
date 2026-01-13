//! Lightweight formatting helpers for timestamps displayed in the browser UI.
//!
//! The History panel shows timestamps for many rows. `chrono`'s general-purpose formatting
//! (`DateTime::format`) is relatively heavy, so we keep the output format fixed and implement a
//! small specialized formatter.

use chrono::{DateTime, Local, TimeZone, Utc};

/// Format a Unix epoch millisecond timestamp for history UI display.
///
/// Output format matches `chrono`'s `"%Y-%m-%d %H:%M"` in the local timezone.
///
/// Returns `None` for unknown/invalid timestamps (`visited_at_ms == 0` or out-of-range).
pub fn format_history_timestamp_ms(visited_at_ms: u64) -> Option<String> {
  if visited_at_ms == 0 {
    return None;
  }

  let ms: i64 = visited_at_ms.try_into().ok()?;
  let utc: DateTime<Utc> = Utc.timestamp_millis_opt(ms).single()?;
  let local = utc.with_timezone(&Local);
  Some(format_local_ymd_hm(&local))
}

fn format_local_ymd_hm(dt: &DateTime<Local>) -> String {
  use chrono::{Datelike, Timelike};

  let year = dt.year();
  // For the normal range of Unix timestamps we expect (1970+), year always fits in 4 digits.
  // If a caller somehow provides a timestamp outside this range, fall back to chrono's formatter
  // to preserve exact semantics (including extended/negative years).
  if !(0..=9999).contains(&year) {
    return dt.format("%Y-%m-%d %H:%M").to_string();
  }

  let mut out = String::with_capacity(16);
  push_4(&mut out, year as u32);
  out.push('-');
  push_2(&mut out, dt.month());
  out.push('-');
  push_2(&mut out, dt.day());
  out.push(' ');
  push_2(&mut out, dt.hour());
  out.push(':');
  push_2(&mut out, dt.minute());
  out
}

#[inline]
fn push_2(out: &mut String, value: u32) {
  // `value` is expected to be in 0..=99.
  let tens = (value / 10) as u8;
  let ones = (value % 10) as u8;
  out.push(char::from(b'0' + tens));
  out.push(char::from(b'0' + ones));
}

#[inline]
fn push_4(out: &mut String, value: u32) {
  let d1 = (value / 1000) as u8;
  let d2 = ((value / 100) % 10) as u8;
  let d3 = ((value / 10) % 10) as u8;
  let d4 = (value % 10) as u8;
  out.push(char::from(b'0' + d1));
  out.push(char::from(b'0' + d2));
  out.push(char::from(b'0' + d3));
  out.push(char::from(b'0' + d4));
}

#[cfg(test)]
mod tests {
  use super::format_history_timestamp_ms;

  #[test]
  fn format_history_timestamp_ms_zero_is_none() {
    assert_eq!(format_history_timestamp_ms(0), None);
  }

  #[test]
  fn format_history_timestamp_ms_matches_chrono_format_on_this_machine() {
    use chrono::{Local, TimeZone, Utc};

    let samples: &[u64] = &[
      1,
      1_000,
      60_000,
      1_692_000_000_000, // 2023-ish
      1_700_000_000_000, // 2023-11-ish
      2_000_000_000_000, // 2033-ish
    ];

    for &ms in samples {
      let expected = {
        let ms_i64: i64 = ms.try_into().expect("sample should fit i64");
        let utc = Utc.timestamp_millis_opt(ms_i64).single().expect("valid ms");
        utc
          .with_timezone(&Local)
          .format("%Y-%m-%d %H:%M")
          .to_string()
      };
      let actual = format_history_timestamp_ms(ms).expect("expected Some(timestamp)");
      assert_eq!(actual, expected, "mismatch for ms={ms}");
    }
  }
}

