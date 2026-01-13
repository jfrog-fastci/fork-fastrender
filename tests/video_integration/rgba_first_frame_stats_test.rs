use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const FIXTURE_WIDTH: usize = 64;
const FIXTURE_HEIGHT: usize = 64;

#[derive(Debug, Clone, Copy)]
struct RgbaStats {
  avg_r: f64,
  avg_g: f64,
  avg_b: f64,
  avg_a: f64,
  center: [u8; 4],
}

fn ffmpeg_available() -> bool {
  Command::new("ffmpeg")
    .arg("-version")
    .output()
    .is_ok_and(|out| out.status.success())
}

fn decode_first_frame_rgba(path: &Path) -> Vec<u8> {
  let out = Command::new("ffmpeg")
    .arg("-nostdin")
    .args(["-v", "error"])
    .arg("-threads")
    .arg("1")
    .arg("-i")
    .arg(path)
    .args(["-map", "0:v:0"])
    .args(["-frames:v", "1"])
    .args(["-f", "rawvideo"])
    .args(["-pix_fmt", "rgba"])
    .arg("-")
    .output()
    .unwrap_or_else(|err| panic!("failed to spawn ffmpeg for {}: {err}", path.display()));

  assert!(
    out.status.success(),
    "ffmpeg failed for {}\nstatus={:?}\nstderr:\n{}",
    path.display(),
    out.status.code(),
    String::from_utf8_lossy(&out.stderr)
  );

  out.stdout
}

fn rgba_stats(pixels: &[u8], width: usize, height: usize) -> RgbaStats {
  assert_eq!(
    pixels.len(),
    width * height * 4,
    "expected raw RGBA buffer length to match {}×{}×4, got {} bytes",
    width,
    height,
    pixels.len()
  );

  let mut sum_r: u64 = 0;
  let mut sum_g: u64 = 0;
  let mut sum_b: u64 = 0;
  let mut sum_a: u64 = 0;
  for px in pixels.chunks_exact(4) {
    sum_r += px[0] as u64;
    sum_g += px[1] as u64;
    sum_b += px[2] as u64;
    sum_a += px[3] as u64;
  }

  let denom = (width * height) as f64;
  let avg_r = sum_r as f64 / denom;
  let avg_g = sum_g as f64 / denom;
  let avg_b = sum_b as f64 / denom;
  let avg_a = sum_a as f64 / denom;

  let cx = width / 2;
  let cy = height / 2;
  let idx = (cy * width + cx) * 4;
  let center = [pixels[idx], pixels[idx + 1], pixels[idx + 2], pixels[idx + 3]];

  RgbaStats {
    avg_r,
    avg_g,
    avg_b,
    avg_a,
    center,
  }
}

fn assert_mostly_red(label: &str, stats: RgbaStats) {
  assert!(
    stats.avg_r > 180.0,
    "{label}: expected avg R to be high, got {:.2} (avg G={:.2}, avg B={:.2}, avg A={:.2}, center={:?})",
    stats.avg_r,
    stats.avg_g,
    stats.avg_b,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_g < 80.0,
    "{label}: expected avg G to be low, got {:.2} (avg R={:.2}, avg B={:.2}, avg A={:.2}, center={:?})",
    stats.avg_g,
    stats.avg_r,
    stats.avg_b,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_b < 80.0,
    "{label}: expected avg B to be low, got {:.2} (avg R={:.2}, avg G={:.2}, avg A={:.2}, center={:?})",
    stats.avg_b,
    stats.avg_r,
    stats.avg_g,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_a > 250.0,
    "{label}: expected avg A to be ~255, got {:.2} (avg R={:.2}, avg G={:.2}, avg B={:.2}, center={:?})",
    stats.avg_a,
    stats.avg_r,
    stats.avg_g,
    stats.avg_b,
    stats.center
  );

  // Dominance (helps catch channel swaps).
  assert!(
    stats.avg_r > stats.avg_g + 100.0,
    "{label}: expected avg R to dominate avg G, got avg R={:.2} avg G={:.2}",
    stats.avg_r,
    stats.avg_g
  );
  assert!(
    stats.avg_r > stats.avg_b + 100.0,
    "{label}: expected avg R to dominate avg B, got avg R={:.2} avg B={:.2}",
    stats.avg_r,
    stats.avg_b
  );

  assert!(
    stats.center[0] > 180 && stats.center[1] < 80 && stats.center[2] < 80 && stats.center[3] > 250,
    "{label}: expected center pixel to be red-dominant, got {:?}",
    stats.center
  );
}

#[test]
fn decode_first_frame_mp4_h264_is_red_dominant() {
  if !ffmpeg_available() {
    eprintln!("skipping decode_first_frame_mp4_h264_is_red_dominant: ffmpeg not available");
    return;
  }

  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let fixture = root.join("tests/fixtures/media/test_h264_aac.mp4");
  let rgba = decode_first_frame_rgba(&fixture);
  let stats = rgba_stats(&rgba, FIXTURE_WIDTH, FIXTURE_HEIGHT);
  assert_mostly_red("mp4/h264 first frame (test_h264_aac.mp4)", stats);
}

#[test]
fn decode_first_frame_webm_vp9_is_red_dominant() {
  if !ffmpeg_available() {
    eprintln!("skipping decode_first_frame_webm_vp9_is_red_dominant: ffmpeg not available");
    return;
  }

  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let fixture = root.join("tests/fixtures/media/test_vp9_opus.webm");
  let rgba = decode_first_frame_rgba(&fixture);
  let stats = rgba_stats(&rgba, FIXTURE_WIDTH, FIXTURE_HEIGHT);
  assert_mostly_red("webm/vp9 first frame (test_vp9_opus.webm)", stats);
}
