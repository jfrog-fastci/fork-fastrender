export const utf8ByteOffsetToUtf16Offset = (text: string, byteOffset: number): number => {
  // `diagnostics::TextRange` uses UTF-8 byte offsets.
  // Monaco/JS string indices are UTF-16 code units.
  //
  // This conversion is intentionally conservative: if `byteOffset` lands in the middle of a
  // multi-byte UTF-8 code point, we clamp to the start of that code point.
  let bytes = 0;
  for (let i = 0; i < text.length; ) {
    const cp = text.codePointAt(i);
    if (cp == undefined) {
      break;
    }
    const utf16Units = cp > 0xffff ? 2 : 1;
    const utf8Bytes = cp <= 0x7f ? 1 : cp <= 0x7ff ? 2 : cp <= 0xffff ? 3 : 4;
    if (bytes + utf8Bytes > byteOffset) {
      return i;
    }
    bytes += utf8Bytes;
    i += utf16Units;
    if (bytes === byteOffset) {
      return i;
    }
  }
  return text.length;
};

