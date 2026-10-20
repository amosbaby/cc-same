#!/usr/bin/env bash
# Build release archives for Linux: the app with its desktop entry and icon, and the CLI.
#
#   scripts/bundle-linux.sh            # -> dist/
#
# Install the app archive with:
#   tar -xzf CC-Same-<version>-linux-<arch>.tar.gz -C ~/.local --strip-components=1
set -euo pipefail
cd "$(dirname "$0")/.."

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
case "$(uname -m)" in
  x86_64 | amd64) arch=x64 ;;
  arm64 | aarch64) arch=arm64 ;;
  *) arch="$(uname -m)" ;;
esac
out="${CC_SAME_OUTPUT:-dist}"
mkdir -p "$out"

cargo build --release --locked -p cc-same
cargo build --release --locked --manifest-path crates/app/Cargo.toml

stage="$(mktemp -d)"
root="$stage/cc-same-$version"
install -Dm755 target/release/cc-same-app "$root/bin/cc-same-app"
install -Dm755 target/release/cc-same "$root/bin/cc-same"
install -Dm644 crates/app/resources/linux/cc-same.desktop "$root/share/applications/cc-same.desktop"
install -Dm644 crates/app/resources/linux/cc-same.png "$root/share/icons/hicolor/512x512/apps/cc-same.png"

tar -C "$stage" -czf "$out/CC-Same-$version-linux-$arch.tar.gz" "cc-same-$version"
tar -C target/release -czf "$out/cc-same-cli-$version-linux-$arch.tar.gz" cc-same
rm -rf "$stage"
ls -1 "$out"
