# Force libaom (used for AVIF decoding via `avif-decode`/`libaom-sys`) to build in portable mode
# when host toolchains do not include an assembler (yasm/nasm).
#
# `libaom-sys` builds libaom from source via CMake. On x86_64, libaom's CMake defaults enable
# assembly optimizations and fail configuration when no assembler is available.
#
# Setting `AOM_TARGET_CPU=generic` disables those optimizations and keeps the build working on
# minimal CI/agent environments.
# Set both the normal variable (used during configure-time logic) and the cache entry (so GUI
# tools and downstream scripts can observe the override).
set(AOM_TARGET_CPU "generic")
set(AOM_TARGET_CPU "generic" CACHE STRING "" FORCE)

# Some libaom build configurations still probe for an assembler even when the target CPU is set to
# `generic`. Be explicit about disabling assembler backends so agent/CI environments without
# yasm/nasm can build deterministically.
set(ENABLE_NASM 0 CACHE BOOL "" FORCE)
set(ENABLE_YASM 0 CACHE BOOL "" FORCE)
set(ENABLE_ASM 0 CACHE BOOL "" FORCE)
