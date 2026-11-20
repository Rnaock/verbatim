#!/bin/sh
# Compilation Windows depuis Linux (voir COMPILER.md).
# Usage : tests/build-windows.sh check|build [args]
# Variables : WINBUILD = dossier préparé (rustsrc/library patché, wincrt/linker.sh).
: "${WINBUILD:?indique le dossier préparé (rustsrc/, wincrt/)}"
export PATH=/usr/lib/rust-1.91/bin:$PATH
export RUSTC_BOOTSTRAP=1
export __CARGO_TESTS_ONLY_SRC_ROOT=$WINBUILD/rustsrc/library
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=$WINBUILD/wincrt/linker.sh
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export RC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-windres
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$WINBUILD/target-win}
cd "$(dirname "$0")/.." && cmd=$1 && shift && cargo $cmd --release -Zbuild-std=std,panic_abort --target x86_64-pc-windows-gnu "$@"
