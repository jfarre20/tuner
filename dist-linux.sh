#!/usr/bin/env bash
# Build a Linux tarball: the tuner binary (wgpu presenter, software decode), README, headlines.
# FFmpeg is linked dynamically against the distro's libav*; users install those with apt/dnf.
# Usage: ./dist-linux.sh   -> dist/tuner-<version>-linux-x86_64.tar.gz
set -euo pipefail
cd "$(dirname "$0")"
ver=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1)
cargo build --release
stage="dist/tuner-$ver-linux-x86_64"
rm -rf "$stage"; mkdir -p "$stage"
cp target/release/tuner README.md assets/headlines.txt "$stage/"
cat > "$stage/START HERE.txt" <<EOF
TUNER $ver (Linux)

Run ./tuner once: it writes tuner_settings.json next to itself (or in
~/.config/tuner) and complains that no video folder is set. Put your folder in
"content_dir", optionally logo / ident / music folders, then run ./tuner again.
Press H for the remote legend, S for the set up menu.

Needs the FFmpeg shared libraries (libavcodec, libavformat, libavutil,
libswresample, libswscale) from your distro, plus Vulkan or OpenGL drivers.
Not on Linux yet: the WeatherStar channel and the graphical set up tool.
EOF
tar -C dist -czf "$stage.tar.gz" "$(basename "$stage")"
echo "built $stage.tar.gz ($(du -h "$stage.tar.gz" | cut -f1))"
