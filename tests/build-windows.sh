#!/bin/sh
# Usage : win4.sh check|build [args]
export PATH=/usr/lib/rust-1.91/bin:$PATH
export RUSTC_BOOTSTRAP=1
export __CARGO_TESTS_ONLY_SRC_ROOT=/tmp/claude-0/-home-claude/c70a5cbf-bbf5-5643-af33-59cf62559b1f/scratchpad/rustsrc/library
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=/tmp/claude-0/-home-claude/c70a5cbf-bbf5-5643-af33-59cf62559b1f/scratchpad/wincrt/linker.sh
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export RC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-windres
cd /tmp/claude-0/-home-claude/c70a5cbf-bbf5-5643-af33-59cf62559b1f/scratchpad/v4 && cmd=$1 && shift && cargo $cmd --release -Zbuild-std=std,panic_abort --target x86_64-pc-windows-gnu "$@"
