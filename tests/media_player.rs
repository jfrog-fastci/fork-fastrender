use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fastrender::media::player::MediaPlayer;
use fastrender::paint::display_list::ImageData;

fn fixture_path(rel: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join(rel)
}

fn hash_frame(img: &ImageData) -> u64 {
  let mut hasher = DefaultHasher::new();
  img.width.hash(&mut hasher);
  img.height.hash(&mut hasher);
  img.pixels.hash(&mut hasher);
  hasher.finish()
}

#[test]
fn webm_vp9_player_produces_frames() {
  let path = fixture_path("pages/fixtures/media_playback/assets/test_vp9_opus.webm");
  let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

  let player = MediaPlayer::open_webm(Cursor::new(bytes)).expect("open webm");
  player.play();

  let start = Instant::now();
  while start.elapsed() < Duration::from_secs(2) {
    player.tick().expect("tick");
    if player.current_frame().is_some() {
      return;
    }
    if let Some(wake) = player.next_wake_after() {
      if !wake.is_zero() {
        std::thread::sleep(wake.min(Duration::from_millis(10)));
      }
    }
  }

  panic!("expected current_frame() to become Some within 2s");
}

#[test]
fn webm_vp9_player_frames_change() {
  let path = fixture_path("pages/fixtures/media_playback/assets/test_vp9_opus.webm");
  let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

  let player = MediaPlayer::open_webm(Cursor::new(bytes)).expect("open webm");
  player.play();

  // Wait for first frame.
  let first = {
    let start = Instant::now();
    loop {
      player.tick().expect("tick");
      if let Some(frame) = player.current_frame() {
        break frame;
      }
      if start.elapsed() > Duration::from_secs(2) {
        panic!("expected first frame within 2s");
      }
    }
  };
  let first_hash = hash_frame(&first);

  let start = Instant::now();
  while start.elapsed() < Duration::from_secs(2) {
    player.tick().expect("tick");
    if let Some(frame) = player.current_frame() {
      if hash_frame(&frame) != first_hash {
        return;
      }
    }

    if let Some(wake) = player.next_wake_after() {
      if !wake.is_zero() {
        std::thread::sleep(wake.min(Duration::from_millis(10)));
      }
    } else {
      std::thread::sleep(Duration::from_millis(1));
    }
  }

  panic!("expected frame to change within 2s");
}

