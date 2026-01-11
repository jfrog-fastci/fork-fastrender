use ahash::{HashMap, HashSet};
use criterion::{criterion_group, criterion_main, Criterion};
use optimize_js::analysis::encoding::analyze_cfg_encoding;
use optimize_js::analysis::liveness::calculate_live_ins;
use optimize_js::analysis::nullability::calculate_nullability;
use optimize_js::analysis::range::analyze_ranges;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, BinOp, Const, Inst};
use parse_js::num::JsNumber;
use std::hint::black_box;

fn build_block(label: u32, temps_per_block: u32) -> Vec<Inst> {
  (0..temps_per_block)
    .map(|offset| {
      let tgt = label * temps_per_block + offset;
      let src = tgt.saturating_sub(1);
      Inst::var_assign(tgt, Arg::Var(src))
    })
    .collect()
}

fn linear_cfg(blocks: u32, temps_per_block: u32) -> Cfg {
  let mut graph = CfgGraph::default();
  let mut bblocks = CfgBBlocks::default();
  for label in 0..blocks {
    if label + 1 < blocks {
      graph.connect(label, label + 1);
    }
    bblocks.add(label, build_block(label, temps_per_block));
  }
  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn loop_cfg(blocks: u32, temps_per_block: u32) -> Cfg {
  let mut graph = CfgGraph::default();
  let mut bblocks = CfgBBlocks::default();
  let exit = blocks;
  bblocks.add(exit, Vec::new());
  for label in 0..blocks {
    let next = if label + 1 < blocks { label + 1 } else { 1 };
    graph.connect(label, next);
    if label % 5 == 0 {
      graph.connect(label, exit);
    }
    bblocks.add(label, build_block(label, temps_per_block));
  }
  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn bench_liveness_linear(c: &mut Criterion) {
  let cfg = linear_cfg(200, 4);
  let inlines = HashMap::default();
  let inlined_vars = HashSet::default();
  c.bench_function("liveness linear 200 blocks", |b| {
    b.iter(|| calculate_live_ins(black_box(&cfg), &inlines, &inlined_vars))
  });
}

fn bench_liveness_loop(c: &mut Criterion) {
  let cfg = loop_cfg(120, 6);
  let inlines = HashMap::default();
  let inlined_vars = HashSet::default();
  c.bench_function("liveness loop with exits", |b| {
    b.iter(|| calculate_live_ins(black_box(&cfg), &inlines, &inlined_vars))
  });
}

fn linear_cfg_with_builder(blocks: u32, build: impl Fn(u32) -> Vec<Inst>) -> Cfg {
  let mut graph = CfgGraph::default();
  let mut bblocks = CfgBBlocks::default();
  for label in 0..blocks {
    if label + 1 < blocks {
      graph.connect(label, label + 1);
    }
    bblocks.add(label, build(label));
  }
  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn bench_range_linear(c: &mut Criterion) {
  let cfg = linear_cfg_with_builder(200, |label| {
    let base = label * 4;
    let mut insts = Vec::new();
    insts.push(Inst::var_assign(
      base,
      Arg::Const(Const::Num(JsNumber(base as f64))),
    ));
    for offset in 1..4 {
      let tgt = base + offset;
      insts.push(Inst::bin(
        tgt,
        Arg::Var(tgt - 1),
        BinOp::Add,
        Arg::Const(Const::Num(JsNumber(1.0))),
      ));
    }
    insts
  });
  c.bench_function("range linear 200 blocks", |b| {
    b.iter(|| black_box(analyze_ranges(black_box(&cfg))))
  });
}

fn bench_nullability_linear(c: &mut Criterion) {
  let cfg = linear_cfg_with_builder(200, |label| {
    let base = label * 4;
    (0..4)
      .map(|offset| {
        let tgt = base + offset;
        let arg = match offset % 3 {
          0 => Arg::Const(Const::Null),
          1 => Arg::Const(Const::Undefined),
          _ => Arg::Var(tgt.saturating_sub(1)),
        };
        Inst::var_assign(tgt, arg)
      })
      .collect()
  });
  c.bench_function("nullability linear 200 blocks", |b| {
    b.iter(|| black_box(calculate_nullability(black_box(&cfg))))
  });
}

fn bench_encoding_linear(c: &mut Criterion) {
  let cfg = linear_cfg_with_builder(200, |label| {
    let base = label * 4;
    (0..4)
      .map(|offset| {
        let tgt = base + offset;
        let s = if offset % 5 == 0 { "abc" } else { "aβc" };
        Inst::var_assign(tgt, Arg::Const(Const::Str(s.to_string())))
      })
      .collect()
  });
  c.bench_function("encoding linear 200 blocks", |b| {
    b.iter(|| black_box(analyze_cfg_encoding(black_box(&cfg))))
  });
}

criterion_group!(
  analysis,
  bench_liveness_linear,
  bench_liveness_loop,
  bench_range_linear,
  bench_nullability_linear,
  bench_encoding_linear
);
criterion_main!(analysis);
