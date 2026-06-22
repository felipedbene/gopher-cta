#!/usr/bin/env bash
# Cross-compile gopher-cta for a big-endian PowerPC64 Linux host (PowerMac G5).
#
# Produces:  dist/gopher-cta-powerpc64   (big-endian PPC64 ELF, OpenSSL-backed TLS)
#
# Why this and not `cross`/`cargo` directly:
#   - ring (rustls' default crypto) has no big-endian support, so the default
#     build can't target the G5. We build with `--features tls-native`, which
#     uses OpenSSL (vendored, compiled from source for the target).
#   - The big-endian powerpc64 toolchain isn't in Debian/Ubuntu's normal repos
#     (only little-endian ppc64el is). The cross-rs image carries one, so we
#     build a small builder image on top of it (see Dockerfile.ppc64) and drive
#     cargo inside it.
#
# Requirements: Docker. On Apple Silicon the build runs under emulation (slow but
# correct). If your Docker stores registry creds in the macOS keychain and the
# keychain is locked in a non-interactive shell, pre-pull the base image once
# from an interactive terminal: docker pull ghcr.io/cross-rs/powerpc64-unknown-linux-gnu:0.2.5
set -euo pipefail

cd "$(dirname "$0")/.."
IMAGE=gopher-ppc64-builder
TARGET=powerpc64-unknown-linux-gnu

echo "== building builder image ($IMAGE) =="
docker build --platform linux/amd64 -t "$IMAGE" -f scripts/Dockerfile.ppc64 scripts/

echo "== compiling (release, tls-native) for $TARGET =="
docker run --rm --platform linux/amd64 \
  -v "$PWD":/work -w /work \
  -e CARGO_TARGET_DIR=/work/target-ppc64 \
  "$IMAGE" \
  cargo build --release --target "$TARGET" --no-default-features --features tls-native

mkdir -p dist
cp "target-ppc64/$TARGET/release/gopher-cta" dist/gopher-cta-powerpc64
echo "== done: dist/gopher-cta-powerpc64 =="
file dist/gopher-cta-powerpc64
