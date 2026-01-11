use ahash::{HashMap, HashSet};
use criterion::{criterion_group, criterion_main, Criterion};
use optimize_js::analysis::analyze_cfg;
use optimize_js::analysis::encoding::analyze_cfg_encoding;
use optimize_js::analysis::liveness::calculate_live_ins;
use optimize_js::analysis::nullability::calculate_nullability;
use optimize_js::analysis::range::analyze_ranges;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, BinOp, Const, Inst, UnOp};
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
    b.iter(|| {
      let result = calculate_live_ins(black_box(&cfg), &inlines, &inlined_vars);
      black_box(result);
    })
  });
}

fn bench_liveness_loop(c: &mut Criterion) {
  let cfg = loop_cfg(120, 6);
  let inlines = HashMap::default();
  let inlined_vars = HashSet::default();
  c.bench_function("liveness loop with exits", |b| {
    b.iter(|| {
      let result = calculate_live_ins(black_box(&cfg), &inlines, &inlined_vars);
      black_box(result);
    })
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
    b.iter(|| {
      let result = analyze_ranges(black_box(&cfg));
      black_box(result);
    })
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
    b.iter(|| {
      let result = calculate_nullability(black_box(&cfg));
      black_box(result);
    })
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
    b.iter(|| {
      let result = analyze_cfg_encoding(black_box(&cfg));
      black_box(result);
    })
  });
}

fn range_loop_cfg(units: u32, temps_per_block: u32) -> Cfg {
  // Loop structure:
  //   preheader -> header -> body... -> latch -> header
  //                      \\-> exit
  //
  // The loop body contains diamond-shaped branches that test `i < K` to exercise
  // edge refinement; the backedge triggers widening in range analysis.
  let preheader = 0u32;
  let header = 1u32;
  let body_start = 2u32;
  let body_blocks = units * 4;
  let latch = body_start + body_blocks;
  let exit = latch + 1;

  let mut graph = CfgGraph::default();
  let mut bblocks = CfgBBlocks::default();

  // Edges: preheader -> header, header -> body/exit
  graph.connect(preheader, header);
  graph.connect(header, body_start);
  graph.connect(header, exit);

  // i0 in preheader, i in header, i_next in latch.
  let i0 = preheader * temps_per_block;
  let i = header * temps_per_block;
  let i_next = latch * temps_per_block;

  bblocks.add(
    preheader,
    vec![Inst::var_assign(i0, Arg::Const(Const::Num(JsNumber(0.0))))],
  );

  let mut phi = Inst::phi_empty(i);
  phi.insert_phi(preheader, Arg::Var(i0));
  phi.insert_phi(latch, Arg::Var(i_next));
  let k_outer = 10_000.0;
  let header_base = header * temps_per_block;
  let cmp = header_base + 1;
  let not = header_base + 2;
  let tmp = header_base + 3;
  bblocks.add(
    header,
    vec![
      phi,
      Inst::bin(
        cmp,
        Arg::Var(i),
        BinOp::Lt,
        Arg::Const(Const::Num(JsNumber(k_outer))),
      ),
      // Invert the condition so range analysis has to look through `!` when
      // applying edge constraints.
      Inst::un(not, UnOp::Not, Arg::Var(cmp)),
      Inst::bin(
        tmp,
        Arg::Var(i),
        BinOp::Add,
        Arg::Const(Const::Num(JsNumber(0.0))),
      ),
      Inst::cond_goto(Arg::Var(not), exit, body_start),
    ],
  );

  // Body: units of [cmp, then, else, join]
  for unit in 0..units {
    let cmp_label = body_start + unit * 4;
    let then_label = cmp_label + 1;
    let else_label = cmp_label + 2;
    let join_label = cmp_label + 3;
    let next_label = if unit + 1 < units {
      body_start + (unit + 1) * 4
    } else {
      latch
    };

    // Edges for the diamond.
    graph.connect(cmp_label, then_label);
    graph.connect(cmp_label, else_label);
    graph.connect(then_label, join_label);
    graph.connect(else_label, join_label);
    graph.connect(join_label, next_label);

    let cmp_base = cmp_label * temps_per_block;
    let then_base = then_label * temps_per_block;
    let else_base = else_label * temps_per_block;
    let join_base = join_label * temps_per_block;

    let c = 50.0 + (unit % 200) as f64;
    let cmp_tmp = cmp_base;
    let cond = cmp_base + 1;
    let cond_not = cmp_base + 2;
    let cmp_out = cmp_base + 3;
    let use_not = unit % 3 == 0;
    let cond_var = if use_not { cond_not } else { cond };
    bblocks.add(
      cmp_label,
      vec![
        Inst::bin(
          cmp_tmp,
          Arg::Var(i),
          BinOp::Add,
          Arg::Const(Const::Num(JsNumber((unit % 4) as f64))),
        ),
        Inst::bin(
          cond,
          Arg::Var(i),
          BinOp::Lt,
          Arg::Const(Const::Num(JsNumber(c))),
        ),
        // Exercise `if (!(i < K))` handling periodically.
        Inst::un(cond_not, UnOp::Not, Arg::Var(cond)),
        Inst::bin(
          cmp_out,
          Arg::Var(cmp_tmp),
          BinOp::Mul,
          Arg::Const(Const::Num(JsNumber(2.0))),
        ),
        Inst::cond_goto(Arg::Var(cond_var), then_label, else_label),
      ],
    );

    // Then/else blocks: simple arithmetic chains.
    bblocks.add(
      then_label,
      vec![
        Inst::bin(
          then_base,
          Arg::Var(i),
          BinOp::Add,
          Arg::Const(Const::Num(JsNumber(1.0))),
        ),
        Inst::bin(
          then_base + 1,
          Arg::Var(then_base),
          BinOp::Mul,
          Arg::Const(Const::Num(JsNumber(2.0))),
        ),
        Inst::bin(
          then_base + 2,
          Arg::Var(then_base + 1),
          BinOp::Sub,
          Arg::Const(Const::Num(JsNumber(3.0))),
        ),
        Inst::var_assign(then_base + 3, Arg::Var(then_base + 2)),
      ],
    );
    bblocks.add(
      else_label,
      vec![
        Inst::bin(
          else_base,
          Arg::Var(i),
          BinOp::Add,
          Arg::Const(Const::Num(JsNumber(5.0))),
        ),
        Inst::bin(
          else_base + 1,
          Arg::Var(else_base),
          BinOp::Mul,
          Arg::Const(Const::Num(JsNumber(3.0))),
        ),
        Inst::bin(
          else_base + 2,
          Arg::Var(else_base + 1),
          BinOp::Sub,
          Arg::Const(Const::Num(JsNumber(7.0))),
        ),
        Inst::var_assign(else_base + 3, Arg::Var(else_base + 2)),
      ],
    );

    // Join block: phi + a couple of arithmetic ops.
    let mut phi = Inst::phi_empty(join_base);
    phi.insert_phi(then_label, Arg::Var(then_base + 3));
    phi.insert_phi(else_label, Arg::Var(else_base + 3));
    bblocks.add(
      join_label,
      vec![
        phi,
        Inst::bin(
          join_base + 1,
          Arg::Var(join_base),
          BinOp::Add,
          Arg::Const(Const::Num(JsNumber(1.0))),
        ),
        Inst::bin(
          join_base + 2,
          Arg::Var(join_base + 1),
          BinOp::Mul,
          Arg::Const(Const::Num(JsNumber(2.0))),
        ),
        Inst::var_assign(join_base + 3, Arg::Var(join_base + 2)),
      ],
    );
  }

  // Latch: i_next = i + 1 and a small arithmetic tail.
  let latch_base = latch * temps_per_block;
  bblocks.add(
    latch,
    vec![
      Inst::bin(
        i_next,
        Arg::Var(i),
        BinOp::Add,
        Arg::Const(Const::Num(JsNumber(1.0))),
      ),
      Inst::bin(
        latch_base + 1,
        Arg::Var(i_next),
        BinOp::Mul,
        Arg::Const(Const::Num(JsNumber(2.0))),
      ),
      Inst::bin(
        latch_base + 2,
        Arg::Var(latch_base + 1),
        BinOp::Sub,
        Arg::Const(Const::Num(JsNumber(3.0))),
      ),
      Inst::var_assign(latch_base + 3, Arg::Var(latch_base + 2)),
    ],
  );
  graph.connect(latch, header);

  // Exit block.
  let exit_base = exit * temps_per_block;
  bblocks.add(exit, vec![Inst::var_assign(exit_base, Arg::Var(i))]);

  Cfg {
    graph,
    bblocks,
    entry: preheader,
  }
}

fn nullability_branches_cfg(steps: u32, temps_per_block: u32) -> Cfg {
  // Repeated diamond patterns:
  //   cond -> then/else -> join -> next cond
  //
  // The condition is `x == null` and periodically inverted via `!tmp` to
  // exercise the nullability refinement logic through boolean negation.
  let mut graph = CfgGraph::default();
  let mut bblocks = CfgBBlocks::default();

  for step in 0..steps {
    let cond_label = step * 4;
    let then_label = cond_label + 1;
    let else_label = cond_label + 2;
    let join_label = cond_label + 3;
    let next_label = if step + 1 < steps { (step + 1) * 4 } else { join_label };

    graph.connect(cond_label, then_label);
    graph.connect(cond_label, else_label);
    graph.connect(then_label, join_label);
    graph.connect(else_label, join_label);
    if step + 1 < steps {
      graph.connect(join_label, next_label);
    }

    let cond_base = cond_label * temps_per_block;
    let then_base = then_label * temps_per_block;
    let else_base = else_label * temps_per_block;
    let join_base = join_label * temps_per_block;

    let x = cond_base;
    let cmp = cond_base + 1;
    let not = cond_base + 2;
    let invert = step % 2 == 0;
    let cond_var = if invert { not } else { cmp };

    let mut cond_insts = vec![
      Inst::unknown_load(x, "x".to_string()),
      Inst::bin(cmp, Arg::Var(x), BinOp::LooseEq, Arg::Const(Const::Null)),
    ];
    if invert {
      cond_insts.push(Inst::un(not, UnOp::Not, Arg::Var(cmp)));
    }
    cond_insts.push(Inst::cond_goto(Arg::Var(cond_var), then_label, else_label));
    bblocks.add(cond_label, cond_insts);

    // Copy the refined value into branch-local vars so the join can merge them.
    bblocks.add(then_label, vec![Inst::var_assign(then_base, Arg::Var(x))]);
    bblocks.add(else_label, vec![Inst::var_assign(else_base, Arg::Var(x))]);

    let mut phi = Inst::phi_empty(join_base);
    phi.insert_phi(then_label, Arg::Var(then_base));
    phi.insert_phi(else_label, Arg::Var(else_base));
    bblocks.add(
      join_label,
      vec![
        phi,
        Inst::var_assign(join_base + 1, Arg::Var(join_base)),
        Inst::var_assign(join_base + 2, Arg::Var(join_base + 1)),
        Inst::var_assign(join_base + 3, Arg::Var(join_base + 2)),
      ],
    );
  }

  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn encoding_template_concat_cfg(blocks: u32, temps_per_block: u32) -> Cfg {
  let mut graph = CfgGraph::default();
  let mut bblocks = CfgBBlocks::default();
  for label in 0..blocks {
    if label + 1 < blocks {
      graph.connect(label, label + 1);
    }

    let base = label * temps_per_block;
    let lit = base;
    let concat1 = base + 1;
    let templ = base + 2;
    let concat2 = base + 3;

    let lit_str = match label % 3 {
      0 => "hello",
      1 => "ÿ", // Latin-1
      _ => "π", // UTF-8
    };

    let prev = if label == 0 {
      Arg::Const(Const::Str("seed".to_string()))
    } else {
      Arg::Var((label - 1) * temps_per_block + 3)
    };

    bblocks.add(
      label,
      vec![
        Inst::var_assign(lit, Arg::Const(Const::Str(lit_str.to_string()))),
        Inst::bin(concat1, prev, BinOp::Add, Arg::Var(lit)),
        Inst::call(
          templ,
          Arg::Builtin("__optimize_js_template".to_string()),
          Arg::Const(Const::Undefined),
          vec![
            Arg::Const(Const::Str("head".to_string())),
            Arg::Var(concat1),
            Arg::Const(Const::Str("tail".to_string())),
          ],
          Vec::new(),
        ),
        Inst::bin(concat2, Arg::Var(templ), BinOp::Add, Arg::Var(lit)),
      ],
    );
  }

  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn analyze_cfg_bundle_cfg() -> Cfg {
  // Bundle CFG that mixes:
  // - a range loop with widening
  // - nullability branch refinement via `x == null` (plus `!tmp` inversion)
  // - encoding tracking through template calls and concatenations
  //
  // This is used for benchmarking the analysis driver wrapper `analyze_cfg`.
  const TEMPS_PER_BLOCK: u32 = 8;

  // Segment sizing.
  let range_units = 20u32; // ~80 blocks in the loop body
  let null_steps = 30u32; // 120 blocks
  let encoding_blocks = 80u32;

  // Range segment label layout.
  let range_preheader = 0u32;
  let range_header = 1u32;
  let range_body_start = 2u32;
  let range_body_blocks = range_units * 4;
  let range_latch = range_body_start + range_body_blocks;
  let range_exit = range_latch + 1;

  // Nullability segment label layout.
  let null_start = range_exit + 1;
  let null_blocks = null_steps * 4;
  let null_end = null_start + null_blocks - 1;

  // Encoding segment label layout.
  let enc_start = null_end + 1;
  let enc_end = enc_start + encoding_blocks - 1;

  let mut graph = CfgGraph::default();
  let mut bblocks = CfgBBlocks::default();

  // ---- Range segment ----
  graph.connect(range_preheader, range_header);
  graph.connect(range_header, range_body_start);
  graph.connect(range_header, range_exit);

  let i0 = range_preheader * TEMPS_PER_BLOCK;
  let i = range_header * TEMPS_PER_BLOCK;
  let i_next = range_latch * TEMPS_PER_BLOCK;
  bblocks.add(
    range_preheader,
    vec![Inst::var_assign(i0, Arg::Const(Const::Num(JsNumber(0.0))))],
  );

  let mut phi = Inst::phi_empty(i);
  phi.insert_phi(range_preheader, Arg::Var(i0));
  phi.insert_phi(range_latch, Arg::Var(i_next));
  let k_outer = 10_000.0;
  let header_base = range_header * TEMPS_PER_BLOCK;
  let cmp = header_base + 1;
  let not = header_base + 2;
  bblocks.add(
    range_header,
    vec![
      phi,
      Inst::bin(
        cmp,
        Arg::Var(i),
        BinOp::Lt,
        Arg::Const(Const::Num(JsNumber(k_outer))),
      ),
      Inst::un(not, UnOp::Not, Arg::Var(cmp)),
      Inst::cond_goto(Arg::Var(not), range_exit, range_body_start),
    ],
  );

  for unit in 0..range_units {
    let cmp_label = range_body_start + unit * 4;
    let then_label = cmp_label + 1;
    let else_label = cmp_label + 2;
    let join_label = cmp_label + 3;
    let next_label = if unit + 1 < range_units {
      range_body_start + (unit + 1) * 4
    } else {
      range_latch
    };

    graph.connect(cmp_label, then_label);
    graph.connect(cmp_label, else_label);
    graph.connect(then_label, join_label);
    graph.connect(else_label, join_label);
    graph.connect(join_label, next_label);

    let cmp_base = cmp_label * TEMPS_PER_BLOCK;
    let then_base = then_label * TEMPS_PER_BLOCK;
    let else_base = else_label * TEMPS_PER_BLOCK;
    let join_base = join_label * TEMPS_PER_BLOCK;

    let c = 50.0 + (unit % 200) as f64;
    let cond = cmp_base + 1;
    bblocks.add(
      cmp_label,
      vec![
        Inst::bin(
          cond,
          Arg::Var(i),
          BinOp::Lt,
          Arg::Const(Const::Num(JsNumber(c))),
        ),
        Inst::cond_goto(Arg::Var(cond), then_label, else_label),
      ],
    );

    bblocks.add(
      then_label,
      vec![Inst::bin(
        then_base,
        Arg::Var(i),
        BinOp::Add,
        Arg::Const(Const::Num(JsNumber(1.0))),
      )],
    );
    bblocks.add(
      else_label,
      vec![Inst::bin(
        else_base,
        Arg::Var(i),
        BinOp::Add,
        Arg::Const(Const::Num(JsNumber(2.0))),
      )],
    );
    let mut phi = Inst::phi_empty(join_base);
    phi.insert_phi(then_label, Arg::Var(then_base));
    phi.insert_phi(else_label, Arg::Var(else_base));
    bblocks.add(join_label, vec![phi]);
  }

  let latch_base = range_latch * TEMPS_PER_BLOCK;
  bblocks.add(
    range_latch,
    vec![Inst::bin(
      i_next,
      Arg::Var(i),
      BinOp::Add,
      Arg::Const(Const::Num(JsNumber(1.0))),
    )],
  );
  graph.connect(range_latch, range_header);

  // Range exit flows into nullability segment.
  graph.connect(range_exit, null_start);
  bblocks.add(
    range_exit,
    vec![Inst::var_assign(latch_base + 1, Arg::Var(i_next))],
  );

  // ---- Nullability segment ----
  for step in 0..null_steps {
    let cond_label = null_start + step * 4;
    let then_label = cond_label + 1;
    let else_label = cond_label + 2;
    let join_label = cond_label + 3;
    let next_label = if step + 1 < null_steps {
      null_start + (step + 1) * 4
    } else {
      enc_start
    };

    graph.connect(cond_label, then_label);
    graph.connect(cond_label, else_label);
    graph.connect(then_label, join_label);
    graph.connect(else_label, join_label);
    graph.connect(join_label, next_label);

    let cond_base = cond_label * TEMPS_PER_BLOCK;
    let then_base = then_label * TEMPS_PER_BLOCK;
    let else_base = else_label * TEMPS_PER_BLOCK;
    let join_base = join_label * TEMPS_PER_BLOCK;

    let x = cond_base;
    let cmp = cond_base + 1;
    let not = cond_base + 2;
    let invert = step % 2 == 0;
    let cond_var = if invert { not } else { cmp };

    let mut cond_insts = vec![
      Inst::unknown_load(x, "x".to_string()),
      Inst::bin(cmp, Arg::Var(x), BinOp::LooseEq, Arg::Const(Const::Null)),
    ];
    if invert {
      cond_insts.push(Inst::un(not, UnOp::Not, Arg::Var(cmp)));
    }
    cond_insts.push(Inst::cond_goto(Arg::Var(cond_var), then_label, else_label));
    bblocks.add(cond_label, cond_insts);

    bblocks.add(then_label, vec![Inst::var_assign(then_base, Arg::Var(x))]);
    bblocks.add(else_label, vec![Inst::var_assign(else_base, Arg::Var(x))]);

    let mut phi = Inst::phi_empty(join_base);
    phi.insert_phi(then_label, Arg::Var(then_base));
    phi.insert_phi(else_label, Arg::Var(else_base));
    bblocks.add(join_label, vec![phi]);
  }

  // ---- Encoding segment ----
  for label in enc_start..=enc_end {
    if label < enc_end {
      graph.connect(label, label + 1);
    }

    let base = label * TEMPS_PER_BLOCK;
    let lit = base;
    let concat1 = base + 1;
    let templ = base + 2;
    let concat2 = base + 3;

    let lit_str = match (label - enc_start) % 3 {
      0 => "hello",
      1 => "ÿ",
      _ => "π",
    };

    let prev = if label == enc_start {
      Arg::Const(Const::Str("seed".to_string()))
    } else {
      Arg::Var((label - 1) * TEMPS_PER_BLOCK + 3)
    };

    bblocks.add(
      label,
      vec![
        Inst::var_assign(lit, Arg::Const(Const::Str(lit_str.to_string()))),
        Inst::bin(concat1, prev, BinOp::Add, Arg::Var(lit)),
        Inst::call(
          templ,
          Arg::Builtin("__optimize_js_template".to_string()),
          Arg::Const(Const::Undefined),
          vec![
            Arg::Const(Const::Str("head".to_string())),
            Arg::Var(concat1),
            Arg::Const(Const::Str("tail".to_string())),
          ],
          Vec::new(),
        ),
        Inst::bin(concat2, Arg::Var(templ), BinOp::Add, Arg::Var(lit)),
      ],
    );
  }

  Cfg {
    graph,
    bblocks,
    entry: range_preheader,
  }
}

fn bench_range_loop(c: &mut Criterion) {
  let cfg = range_loop_cfg(50, 4);
  c.bench_function("range loop with widening", |b| {
    b.iter(|| {
      let result = analyze_ranges(black_box(&cfg));
      black_box(result);
    })
  });
}

fn bench_nullability_branches(c: &mut Criterion) {
  let cfg = nullability_branches_cfg(60, 4);
  c.bench_function("nullability branches with inversion", |b| {
    b.iter(|| {
      let result = calculate_nullability(black_box(&cfg));
      black_box(result);
    })
  });
}

fn bench_encoding_template_concat(c: &mut Criterion) {
  let cfg = encoding_template_concat_cfg(260, 4);
  c.bench_function("encoding template + concat", |b| {
    b.iter(|| {
      let result = analyze_cfg_encoding(black_box(&cfg));
      black_box(result);
    })
  });
}

fn bench_analyze_cfg_bundle(c: &mut Criterion) {
  let cfg = analyze_cfg_bundle_cfg();
  c.bench_function("analyze_cfg bundle", |b| {
    b.iter(|| {
      let result = analyze_cfg(black_box(&cfg));
      black_box(result);
    })
  });
}

criterion_group!(
  analysis,
  bench_liveness_linear,
  bench_liveness_loop,
  bench_range_linear,
  bench_range_loop,
  bench_nullability_linear,
  bench_nullability_branches,
  bench_encoding_linear,
  bench_encoding_template_concat,
  bench_analyze_cfg_bundle
);
criterion_main!(analysis);
