import { describe, expect, test } from "vitest";
import {
  NormalizedStep,
  blockMatchesQuery,
  computeChangedBlocks,
  formatForeign,
  formatId,
  normalizeStep,
  parseProgramDump,
  StableDebugStep,
  StableInst,
} from "./schema";

const sampleInst = (t: string): StableInst => ({
  t,
  tgts: [0],
  args: [],
  spreads: [],
  labels: [],
});

const buildStep = (name: string, inst: StableInst): StableDebugStep => ({
  name,
  bblockOrder: [0],
  bblocks: {
    0: [inst],
  },
  cfgChildren: {
    0: [],
  },
});

describe("formatId", () => {
  test("renders numeric IDs", () => {
    expect(formatId({ type: "number", value: "18446744073709551616" })).toBe(
      "18446744073709551616",
    );
  });

  test("renders text IDs", () => {
    expect(formatId({ type: "text", value: "sym-1" })).toBe("sym-1");
  });
});

describe("formatForeign", () => {
  test("renders string IDs", () => {
    expect(formatForeign("18446744073709551616")).toBe("18446744073709551616");
  });
});

describe("parseProgramDump", () => {
  test("accepts foreignStr in /compile_dump instructions", () => {
    const dump = parseProgramDump({
      version: 1,
      sourceMode: "module",
      topLevel: {
        params: [],
        cfg: {
          entry: 0,
          bblockOrder: [0],
          bblocks: {
            0: [
              {
                t: "ForeignLoad",
                tgts: [0],
                args: [],
                spreads: [],
                labels: [],
                foreign: 0,
                foreignStr: "18446744073709551616",
                meta: {
                  effects: { reads: [], writes: [], summary: {}, unknown: false },
                  purity: "unknown",
                  calleePurity: "unknown",
                  ownership: "unknown",
                  argUseModes: [],
                  excludesNullish: false,
                },
              },
            ],
          },
          cfgEdges: {
            0: [],
          },
        },
      },
      functions: [],
    });

    const inst = dump.topLevel.cfg.bblocks.get(0)?.[0] as any;
    expect(inst.foreignStr).toBe("18446744073709551616");
  });
});

describe("blockMatchesQuery", () => {
  test("uses foreignStr for foreign symbol name lookups", () => {
    // 2^53 + 1, which cannot be represented exactly as a JS number.
    const foreign = 9007199254740993;
    const foreignStr = "9007199254740993";
    const symbolNames = new Map([[foreignStr, "foreignName"]]);

    const matches = blockMatchesQuery(
      {
        label: 0,
        insts: [
          {
            t: "ForeignLoad",
            tgts: [0],
            args: [],
            spreads: [],
            labels: [],
            foreign,
            foreignStr,
            meta: {} as any,
          },
        ],
      },
      "foreignname",
      symbolNames,
    );

    expect(matches).toBe(true);
  });
});

describe("normalizeStep", () => {
  test("converts map keys to numbers", () => {
    const step = normalizeStep(buildStep("demo", sampleInst("Bin")));
    expect(step.blocks[0].label).toBe(0);
    expect([...step.children.keys()]).toEqual([0]);
  });
});

describe("computeChangedBlocks", () => {
  test("detects differences between steps", () => {
    const steps: NormalizedStep[] = [
      normalizeStep(buildStep("a", sampleInst("Bin"))),
      normalizeStep(buildStep("b", sampleInst("Bin"))),
      normalizeStep(buildStep("c", sampleInst("Phi"))),
    ];
    const changes = computeChangedBlocks(steps);
    expect(changes[0].has(0)).toBe(true);
    expect(changes[1].size).toBe(0);
    expect(changes[2].has(0)).toBe(true);
  });
});
