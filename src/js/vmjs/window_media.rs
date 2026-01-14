//! Minimal host-side HTMLMediaElement state registry.
//!
//! FastRender's DOM node wrappers are GC-managed JS objects, while most per-element backing state
//! lives on the host side (Rust). When pages create and discard many media elements, we must ensure
//! any host-side registry entries are eventually removed; otherwise the per-realm registry can grow
//! without bound.
//!
//! The registry is keyed by [`DomNodeKey`] (document id + node id) to avoid collisions across
//! multiple documents in the same JS realm.

use crate::dom2;
use crate::js::dom_platform::{DocumentId, DomNodeKey, DomPlatform};
use crate::media::clock::{MediaClock, PlaybackState};
use crate::media::MediaPlaybackControl;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use vm_js::Heap;

// HTMLMediaElement state constants (mirroring the web-exposed IDL constants).
pub(crate) const NETWORK_EMPTY: u16 = 0;
pub(crate) const NETWORK_IDLE: u16 = 1;
pub(crate) const NETWORK_LOADING: u16 = 2;
pub(crate) const NETWORK_NO_SOURCE: u16 = 3;

pub(crate) const HAVE_NOTHING: u16 = 0;
pub(crate) const HAVE_METADATA: u16 = 1;
pub(crate) const HAVE_CURRENT_DATA: u16 = 2;
pub(crate) const HAVE_FUTURE_DATA: u16 = 3;
pub(crate) const HAVE_ENOUGH_DATA: u16 = 4;

/// Minimal backing state for an `HTMLMediaElement` (`<audio>` / `<video>`).
///
/// This intentionally does **not** store any GC-managed `Value`/`GcObject` handles; the state should
/// remain cheap to keep on the host side.
#[derive(Debug)]
pub(crate) struct MediaElementState {
  playback: Arc<MediaPlaybackControl>,
  muted: bool,
  muted_overridden: bool,
  volume: f64,
  /// The last resolved media URL for this element's current `src` attribute.
  ///
  /// This is used by embedder-driven media discovery (`BrowserTabHost::discover_and_start_media_loads`)
  /// to avoid re-firing load events when the DOM mutation generation changes but the `src` remains
  /// stable.
  pub(crate) src_url: Option<String>,
  pub(crate) network_state: u16,
  pub(crate) ready_state: u16,
  pub(crate) seeking: bool,
  pub(crate) duration: f64,
  pub(crate) autoplay_attempted: bool,
  /// `HTMLMediaElement.error?.code` when set.
  pub(crate) error_code: Option<u16>,
}

impl MediaElementState {
  pub(crate) fn new(master_clock: Arc<dyn MediaClock>) -> Self {
    let playback = Arc::new(MediaPlaybackControl::new(master_clock));
    Self {
      playback,
      muted: false,
      muted_overridden: false,
      volume: 1.0,
      src_url: None,
      network_state: NETWORK_EMPTY,
      ready_state: HAVE_NOTHING,
      seeking: false,
      duration: f64::NAN,
      autoplay_attempted: false,
      error_code: None,
    }
  }

  pub(crate) fn paused(&self) -> bool {
    matches!(self.playback.state(), PlaybackState::Paused)
  }

  pub(crate) fn current_time_seconds(&self) -> f64 {
    self.playback.now().as_secs_f64()
  }

  pub(crate) fn seek(&self, time: Duration) {
    self.playback.seek(time);
  }

  pub(crate) fn play(&self) {
    self.playback.play();
  }

  pub(crate) fn pause(&self) {
    self.playback.pause();
  }

  pub(crate) fn playback_rate(&self) -> f64 {
    self.playback.rate()
  }

  pub(crate) fn set_playback_rate(&self, rate: f64) {
    self.playback.set_rate(rate);
  }

  #[must_use]
  pub(crate) fn playback_control(&self) -> Arc<MediaPlaybackControl> {
    Arc::clone(&self.playback)
  }

  pub(crate) fn muted(&self) -> bool {
    self.muted
  }

  pub(crate) fn muted_effective(&self, muted_attr: bool) -> bool {
    if self.muted_overridden {
      self.muted
    } else {
      muted_attr
    }
  }

  pub(crate) fn set_muted(&mut self, muted: bool) {
    self.muted = muted;
    self.muted_overridden = true;
  }

  pub(crate) fn volume(&self) -> f64 {
    self.volume
  }

  pub(crate) fn set_volume(&mut self, volume: f64) {
    self.volume = volume;
  }
}

#[derive(Debug, Default)]
pub(crate) struct MediaElementStateRegistry {
  last_gc_runs: u64,
  states: HashMap<DomNodeKey, MediaElementState>,
}

impl MediaElementStateRegistry {
  pub(crate) fn len(&self) -> usize {
    self.states.len()
  }

  #[cfg(test)]
  pub(crate) fn is_empty(&self) -> bool {
    self.states.is_empty()
  }

  pub(crate) fn get_or_create(
    &mut self,
    key: DomNodeKey,
    master_clock: &Arc<dyn MediaClock>,
  ) -> &mut MediaElementState {
    self
      .states
      .entry(key)
      .or_insert_with(|| MediaElementState::new(Arc::clone(master_clock)))
  }

  /// Opportunistically sweep unreachable media element state entries.
  ///
  /// Sweeping is triggered by `vm-js` GC runs: whenever `heap.gc_runs()` changes, we remove entries
  /// that are no longer needed.
  ///
  /// Removal conditions (best effort):
  /// - The corresponding DOM wrapper is no longer reachable (collected), as indicated by
  ///   [`DomPlatform`] wrapper caches.
  /// - The node id is no longer valid in the corresponding `dom2::Document`.
  pub(crate) fn sweep_if_needed(
    &mut self,
    heap: &Heap,
    dom_platform: Option<&mut DomPlatform>,
    host_dom: Option<&dom2::Document>,
    owned_dom2_documents: Option<&HashMap<DocumentId, Box<dom2::Document>>>,
  ) -> bool {
    let gc_runs = heap.gc_runs();
    if gc_runs == self.last_gc_runs {
      return false;
    }
    self.last_gc_runs = gc_runs;

    if self.states.is_empty() {
      return true;
    }

    let mut dom_platform = dom_platform;
    self.states.retain(|key, _state| {
      // 1) Wrapper reachability (primary bound): if the wrapper has been collected, drop host-side
      // state.
      if let Some(platform) = dom_platform.as_deref_mut() {
        if platform
          .get_existing_wrapper_for_document_id(heap, key.document_id, key.node_id)
          .is_none()
        {
          return false;
        }
      }

      // 2) Node validity: if the node id is out-of-bounds for the backing `dom2::Document`, drop the
      // entry to avoid accumulating stale state (e.g. after document replacement/remapping).
      let dom = owned_dom2_documents
        .and_then(|docs| docs.get(&key.document_id).map(|dom| dom.as_ref()))
        .or(host_dom);
      if let Some(dom) = dom {
        if dom.node_id_from_index(key.node_id.index()).is_err() {
          return false;
        }
      }

      true
    });

    true
  }
}
