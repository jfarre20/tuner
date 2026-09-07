#!/usr/bin/env bash
# Dev helper for building in WSL/Linux: pkg-config ffmpeg, libclang from the distro's LLVM.
set -uo pipefail
cd "$(dirname "$0")"
rm -f .cargo/config.toml
export PATH="$HOME/.cargo/bin:$PATH"
export LIBCLANG_PATH="$(dirname "$(ls /usr/lib/llvm-*/lib/libclang.so* | head -1)")"
echo "libclang: $LIBCLANG_PATH"
cargo build --release 2>&1 | grep -E "^(error|warning)|Finished|-->" -A 8 | head -200
echo LINUX_BUILD_DONE
ls -la target/release/tuner target/release/tuner-setup target/release/launcher 2>&1
