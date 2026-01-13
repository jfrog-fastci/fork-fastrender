use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
  F32,
  I16,
  U16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleLayout {
  Interleaved,
  Planar,
}

/// Backwards-compatible alias for [`SampleLayout`].
pub type ChannelLayout = SampleLayout;

#[derive(Debug, Clone, Copy)]
pub enum AudioSamples<'a> {
  InterleavedF32(&'a [f32]),
  InterleavedI16(&'a [i16]),
  InterleavedU16(&'a [u16]),
  PlanarF32(&'a [&'a [f32]]),
  PlanarI16(&'a [&'a [i16]]),
  PlanarU16(&'a [&'a [u16]]),
}

impl AudioSamples<'_> {
  pub fn format(&self) -> SampleFormat {
    match self {
      AudioSamples::InterleavedF32(_) | AudioSamples::PlanarF32(_) => SampleFormat::F32,
      AudioSamples::InterleavedI16(_) | AudioSamples::PlanarI16(_) => SampleFormat::I16,
      AudioSamples::InterleavedU16(_) | AudioSamples::PlanarU16(_) => SampleFormat::U16,
    }
  }

  pub fn layout(&self) -> SampleLayout {
    match self {
      AudioSamples::InterleavedF32(_)
      | AudioSamples::InterleavedI16(_)
      | AudioSamples::InterleavedU16(_) => ChannelLayout::Interleaved,
      AudioSamples::PlanarF32(_) | AudioSamples::PlanarI16(_) | AudioSamples::PlanarU16(_) => {
        ChannelLayout::Planar
      }
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioBuffer<'a> {
  pub format: SampleFormat,
  pub layout: SampleLayout,
  pub channels: usize,
  pub sample_rate: u32,
  pub pts: Option<Duration>,
  pub data: AudioSamples<'a>,
}

impl<'a> AudioBuffer<'a> {
  pub fn new(
    channels: usize,
    sample_rate: u32,
    pts: Option<Duration>,
    data: AudioSamples<'a>,
  ) -> Self {
    Self {
      format: data.format(),
      layout: data.layout(),
      channels,
      sample_rate,
      pts,
      data,
    }
  }
}
