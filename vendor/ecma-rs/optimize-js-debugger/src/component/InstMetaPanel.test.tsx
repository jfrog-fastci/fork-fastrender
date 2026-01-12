import { describe, expect, test } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { InstMetaPanel } from "./InstMetaPanel";
import type { GraphInst } from "./schema";

describe("InstMetaPanel", () => {
  test("renders instruction metadata", () => {
    const inst: GraphInst = {
      t: "Call",
      tgts: [1],
      args: [],
      spreads: [],
      labels: [],
      meta: {
        effects: {
          reads: ["Heap"],
          writes: [{ Foreign: 1 }],
          summary: {},
          unknown: true,
        },
        purity: "Impure",
        calleePurity: "Impure",
        ownership: "Owned",
        argUseModes: ["Borrow", "Consume"],
        resultEscape: "return_escape",
        range: { Interval: { lo: { I64: 0 }, hi: { I64: 10 } } },
        nullability: {
          mayBeNull: false,
          mayBeUndefined: false,
          mayBeOther: true,
          isBottom: false,
        },
        encoding: "utf8",
        excludesNullish: true,
        typeId: "123",
        nativeLayout: "0x00000000000000000000000000000000",
        typeSummary: "Number",
        hirExpr: 7,
      },
    };

    const html = renderToStaticMarkup(<InstMetaPanel inst={inst} />);
    expect(html).toMatchSnapshot();
  });
});
