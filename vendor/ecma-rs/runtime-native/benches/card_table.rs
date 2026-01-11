use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rand::prelude::*;
use std::hint::black_box;

const OBJECT_BYTES: usize = 1024 * 1024; // 1 MiB
const PTR_BYTES: usize = core::mem::size_of::<usize>();
const SLOTS: usize = OBJECT_BYTES / PTR_BYTES;

// Simulated mutator writes into an old-gen pointer array.
const WRITE_COUNT: usize = 16 * 1024;

// Fraction of pointer slots which (synthetically) point into the nursery.
//
// We keep this small so the "rebuild after scan" benchmark represents the
// common case where old->young pointers are sparse.
const YOUNG_PTR_RATE: f64 = 0.01;

#[derive(Clone)]
struct ByteCardTable {
  cards: Vec<u8>,
}

impl ByteCardTable {
  fn new(card_count: usize) -> Self {
    Self {
      cards: vec![0u8; card_count],
    }
  }

  #[inline(always)]
  fn clear(&mut self) {
    self.cards.fill(0);
  }

  #[inline(always)]
  fn mark(&mut self, card: usize) {
    // In a write barrier we prefer a single store rather than load/branch.
    // Safety: caller ensures `card < self.cards.len()`.
    unsafe {
      *self.cards.get_unchecked_mut(card) = 1;
    }
  }
}

#[derive(Clone)]
struct BitsetCardTable {
  words: Vec<u64>,
  card_count: usize,
}

impl BitsetCardTable {
  fn new(card_count: usize) -> Self {
    let word_count = card_count.div_ceil(64);
    Self {
      words: vec![0u64; word_count],
      card_count,
    }
  }

  #[inline(always)]
  fn clear(&mut self) {
    self.words.fill(0);
  }

  #[inline(always)]
  fn mark(&mut self, card: usize) {
    debug_assert!(card < self.card_count);
    let word = card / 64;
    let bit = card % 64;
    // Safety: `word` is in-bounds by construction.
    unsafe {
      *self.words.get_unchecked_mut(word) |= 1u64 << bit;
    }
  }
}

#[derive(Copy, Clone, Debug)]
enum Repr {
  Byte,
  Bitset,
}

impl Repr {
  fn label(self) -> &'static str {
    match self {
      Self::Byte => "byte",
      Self::Bitset => "bitset",
    }
  }
}

fn make_write_indices() -> Vec<usize> {
  let mut rng = StdRng::seed_from_u64(0x7b45_cad2_9e9c_4d21);
  (0..WRITE_COUNT)
    .map(|_| rng.random_range(0..SLOTS))
    .collect()
}

fn make_ptr_array() -> Vec<usize> {
  let mut rng = StdRng::seed_from_u64(0xdec0_adde_1bad_f00d);
  (0..SLOTS)
    .map(|_| {
      // `rand` 0.9 doesn't implement `StandardUniform` for `usize` on all
      // platforms; generate from a fixed-width integer instead.
      let base = (rng.random::<u64>() as usize) & !1;
      if rng.random_bool(YOUNG_PTR_RATE) {
        base | 1
      } else {
        base
      }
    })
    .collect()
}

fn build_dirty_cards(card_count: usize, dirty_rate: f64, seed: u64) -> Vec<usize> {
  let dirty_count = (card_count as f64 * dirty_rate).round().clamp(1.0, card_count as f64) as usize;
  let mut cards: Vec<usize> = (0..card_count).collect();
  let mut rng = StdRng::seed_from_u64(seed);
  cards.shuffle(&mut rng);
  cards.truncate(dirty_count);
  cards
}

#[inline(always)]
fn scan_card(ptrs: &[usize], start_slot: usize, end_slot: usize) -> usize {
  let mut acc = 0usize;
  for &value in &ptrs[start_slot..end_slot] {
    acc = acc.wrapping_add(value);
  }
  acc
}

#[inline(always)]
fn scan_card_keep_dirty(ptrs: &[usize], start_slot: usize, end_slot: usize) -> (usize, bool) {
  let mut acc = 0usize;
  let mut keep_dirty = false;
  for &value in &ptrs[start_slot..end_slot] {
    acc = acc.wrapping_add(value);
    keep_dirty |= (value & 1) != 0;
  }
  (acc, keep_dirty)
}

fn bench_mark_random_slots(c: &mut Criterion) {
  let write_indices = make_write_indices();
  let mut group = c.benchmark_group("runtime-native/card_table/mark_random_slots");
  group.throughput(Throughput::Elements(WRITE_COUNT as u64));

  for &card_size in &[128usize, 512, 1024] {
    let card_count = OBJECT_BYTES.div_ceil(card_size);
    let card_shift = card_size.trailing_zeros();
    debug_assert_eq!(card_size, 1usize << card_shift);

    for repr in [Repr::Byte, Repr::Bitset] {
      match repr {
        Repr::Byte => {
          let mut table = ByteCardTable::new(card_count);
          group.bench_function(
            BenchmarkId::new(format!("{}/{}B", repr.label(), card_size), "1MiB"),
            |b| {
              b.iter(|| {
                for &slot in &write_indices {
                  let byte_off = slot * PTR_BYTES;
                  let card = byte_off >> card_shift;
                  table.mark(card);
                }
                black_box(&table);
              });
            },
          );
        }
        Repr::Bitset => {
          let mut table = BitsetCardTable::new(card_count);
          group.bench_function(
            BenchmarkId::new(format!("{}/{}B", repr.label(), card_size), "1MiB"),
            |b| {
              b.iter(|| {
                for &slot in &write_indices {
                  let byte_off = slot * PTR_BYTES;
                  let card = byte_off >> card_shift;
                  table.mark(card);
                }
                black_box(&table);
              });
            },
          );
        }
      }
    }
  }

  group.finish();
}

fn bench_scan_dirty_cards(c: &mut Criterion) {
  let ptrs = make_ptr_array();
  let mut group = c.benchmark_group("runtime-native/card_table/scan_dirty_cards");

  for &card_size in &[128usize, 512, 1024] {
    let card_count = OBJECT_BYTES.div_ceil(card_size);
    let slots_per_card = card_size / PTR_BYTES;

    for &(dirty_rate, label) in &[(0.01, "1%"), (0.10, "10%"), (0.50, "50%")] {
      let dirty_cards = build_dirty_cards(card_count, dirty_rate, 0xfeed_f00d ^ (card_size as u64));
      group.throughput(Throughput::Bytes((OBJECT_BYTES as f64 * dirty_rate) as u64));

      for repr in [Repr::Byte, Repr::Bitset] {
        match repr {
          Repr::Byte => {
            let mut table = ByteCardTable::new(card_count);
            for &card in &dirty_cards {
              table.mark(card);
            }

            group.bench_function(
              BenchmarkId::new(format!("{}/{}B/dirty={}", repr.label(), card_size, label), "1MiB"),
              |b| {
                b.iter(|| {
                  let mut acc = 0usize;
                  for (card, &flag) in table.cards.iter().enumerate() {
                    if flag == 0 {
                      continue;
                    }
                    let start_slot = card * slots_per_card;
                    let end_slot = (start_slot + slots_per_card).min(ptrs.len());
                    acc = acc.wrapping_add(scan_card(&ptrs, start_slot, end_slot));
                  }
                  black_box(acc);
                });
              },
            );
          }
          Repr::Bitset => {
            let mut table = BitsetCardTable::new(card_count);
            for &card in &dirty_cards {
              table.mark(card);
            }

            group.bench_function(
              BenchmarkId::new(format!("{}/{}B/dirty={}", repr.label(), card_size, label), "1MiB"),
              |b| {
                b.iter(|| {
                  let mut acc = 0usize;
                  for (word_idx, &word) in table.words.iter().enumerate() {
                    let mut bits = word;
                    while bits != 0 {
                      let bit = bits.trailing_zeros() as usize;
                      let card = word_idx * 64 + bit;
                      if card >= table.card_count {
                        break;
                      }
                      let start_slot = card * slots_per_card;
                      let end_slot = (start_slot + slots_per_card).min(ptrs.len());
                      acc = acc.wrapping_add(scan_card(&ptrs, start_slot, end_slot));
                      bits &= bits - 1;
                    }
                  }
                  black_box(acc);
                });
              },
            );
          }
        }
      }
    }
  }

  group.finish();
}

fn bench_rebuild_after_scan(c: &mut Criterion) {
  let ptrs = make_ptr_array();
  let mut group = c.benchmark_group("runtime-native/card_table/rebuild_after_scan");

  for &card_size in &[128usize, 512, 1024] {
    let card_count = OBJECT_BYTES.div_ceil(card_size);
    let slots_per_card = card_size / PTR_BYTES;

    for &(dirty_rate, label) in &[(0.01, "1%"), (0.10, "10%"), (0.50, "50%")] {
      let dirty_cards = build_dirty_cards(card_count, dirty_rate, 0xbeef_cafe ^ (card_size as u64));

      for repr in [Repr::Byte, Repr::Bitset] {
        match repr {
          Repr::Byte => {
            let mut input = ByteCardTable::new(card_count);
            let mut output = ByteCardTable::new(card_count);
            for &card in &dirty_cards {
              input.mark(card);
            }

            group.bench_function(
              BenchmarkId::new(
                format!("{}/{}B/dirty={}", repr.label(), card_size, label),
                "1MiB",
              ),
              |b| {
                b.iter(|| {
                  output.clear();
                  let mut acc = 0usize;
                  for (card, &flag) in input.cards.iter().enumerate() {
                    if flag == 0 {
                      continue;
                    }
                    let start_slot = card * slots_per_card;
                    let end_slot = (start_slot + slots_per_card).min(ptrs.len());
                    let (card_sum, keep_dirty) = scan_card_keep_dirty(&ptrs, start_slot, end_slot);
                    acc = acc.wrapping_add(card_sum);
                    if keep_dirty {
                      output.mark(card);
                    }
                  }
                  black_box(acc);
                  black_box(&output);
                });
              },
            );
          }
          Repr::Bitset => {
            let mut input = BitsetCardTable::new(card_count);
            let mut output = BitsetCardTable::new(card_count);
            for &card in &dirty_cards {
              input.mark(card);
            }

            group.bench_function(
              BenchmarkId::new(
                format!("{}/{}B/dirty={}", repr.label(), card_size, label),
                "1MiB",
              ),
              |b| {
                b.iter(|| {
                  output.clear();
                  let mut acc = 0usize;
                  for (word_idx, &word) in input.words.iter().enumerate() {
                    let mut bits = word;
                    while bits != 0 {
                      let bit = bits.trailing_zeros() as usize;
                      let card = word_idx * 64 + bit;
                      if card >= input.card_count {
                        break;
                      }
                      let start_slot = card * slots_per_card;
                      let end_slot = (start_slot + slots_per_card).min(ptrs.len());
                      let (card_sum, keep_dirty) =
                        scan_card_keep_dirty(&ptrs, start_slot, end_slot);
                      acc = acc.wrapping_add(card_sum);
                      if keep_dirty {
                        output.mark(card);
                      }
                      bits &= bits - 1;
                    }
                  }
                  black_box(acc);
                  black_box(&output);
                });
              },
            );
          }
        }
      }
    }
  }

  group.finish();
}

criterion_group!(
  benches,
  bench_mark_random_slots,
  bench_scan_dirty_cards,
  bench_rebuild_after_scan
);
criterion_main!(benches);
