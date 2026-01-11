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
