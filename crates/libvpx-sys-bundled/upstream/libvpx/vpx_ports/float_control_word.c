#include <float.h>

// libvpx expects these symbols to exist for MSVC x86_64 builds (Windows x64 does
// not support inline `__asm`). Upstream libvpx ships a yasm/nasm implementation
// (`float_control_word.asm`), but we prefer a portable C-only build by default.
//
// libvpx only uses these helpers to temporarily force x87 "double precision"
// (53-bit) during encoding for consistency with SSE. In this repo we build a
// decoder-focused libvpx, but `vpx_encoder.c` is still compiled as part of the
// public API, so these symbols must be present at link time.
//
// We implement precision control via `_controlfp_s`. We intentionally only
// modify the precision-control field; callers preserve and restore the previous
// value themselves.

void vpx_winx64_fldcw(unsigned short mode) {
  unsigned int current;
  unsigned int pc;

  // x87 control word precision-control field (bits 8..9):
  //   00b: 24-bit (single precision)
  //   10b: 53-bit (double precision)
  //   11b: 64-bit (extended precision)
  switch (mode & 0x0300u) {
    case 0x0000u:
      pc = _PC_24;
      break;
    case 0x0200u:
      pc = _PC_53;
      break;
    case 0x0300u:
      pc = _PC_64;
      break;
    default:
      // 01b is reserved by x87. Fall back to 53-bit.
      pc = _PC_53;
      break;
  }

  // Only touch precision-control bits; keep the rest unchanged.
  _controlfp_s(&current, pc, _MCW_PC);
}

unsigned short vpx_winx64_fstcw(void) {
  unsigned int cw = 0;
  unsigned short mode = 0;

  _controlfp_s(&cw, 0, 0);

  switch (cw & _MCW_PC) {
    case _PC_24:
      mode |= 0x0000u;
      break;
    case _PC_53:
      mode |= 0x0200u;
      break;
    case _PC_64:
      mode |= 0x0300u;
      break;
    default:
      // If the toolchain doesn't report a recognizable value, assume the
      // default (53-bit).
      mode |= 0x0200u;
      break;
  }

  return mode;
}
