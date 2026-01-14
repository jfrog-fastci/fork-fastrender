// Typed arrays (including BigInt kinds) + resizable ArrayBuffer.
try {
  const buf = new ArrayBuffer(8, { maxByteLength: 32 });
  const i64 = new BigInt64Array(buf);
  i64[0] = 1n;

  buf.resize(16);
  i64[1] = -2n;

  const u8 = new Uint8Array(buf);
  u8[0] = 255;
  u8[1] = 0;

  const view = new DataView(buf);
  view.getInt8(0);
} catch (e) {}

