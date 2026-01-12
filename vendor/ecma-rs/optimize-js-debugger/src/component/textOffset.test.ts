import { describe, expect, test } from "vitest";
import { utf8ByteOffsetToUtf16Offset } from "./textOffset";

describe("utf8ByteOffsetToUtf16Offset", () => {
  test("handles ASCII", () => {
    expect(utf8ByteOffsetToUtf16Offset("abc", 0)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset("abc", 1)).toBe(1);
    expect(utf8ByteOffsetToUtf16Offset("abc", 3)).toBe(3);
  });

  test("handles multibyte UTF-8 (U+00E9)", () => {
    const s = "éa";
    // "é" is 2 bytes in UTF-8 and 1 UTF-16 code unit.
    expect(utf8ByteOffsetToUtf16Offset(s, 0)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset(s, 2)).toBe(1);
    expect(utf8ByteOffsetToUtf16Offset(s, 3)).toBe(2);
  });

  test("handles surrogate pairs (U+1F600)", () => {
    const s = "😀x";
    // 😀 is 4 bytes in UTF-8 and 2 UTF-16 code units.
    expect(utf8ByteOffsetToUtf16Offset(s, 0)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset(s, 4)).toBe(2);
    expect(utf8ByteOffsetToUtf16Offset(s, 5)).toBe(3);
  });
});

