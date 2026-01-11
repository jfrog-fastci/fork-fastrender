# CMake toolchain file used by `scripts/cargo_agent.sh` for agent/CI builds.
#
# `libaom-sys` (used for AVIF decoding) enables x86_64 assembly by default and
# errors out when `yasm`/`nasm` is not available. Force the portable target so
# libaom builds without an assembler.
#
# This file is intentionally minimal so it can be passed to other CMake-based
# sys crates without affecting their configuration (unknown cache variables are
# ignored).

set(AOM_TARGET_CPU "generic" CACHE STRING "Force portable libaom build (no yasm/nasm)")

