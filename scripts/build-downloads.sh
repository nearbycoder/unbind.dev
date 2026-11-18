#!/bin/sh
set -eu

cd "$(dirname "$0")/.."

target_dir=target/downloads
mkdir -p dist

build_linux() {
    cargo zigbuild --locked --release --bin unbind --target-dir "$target_dir" \
        --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl
    install -m 755 "$target_dir/x86_64-unknown-linux-musl/release/unbind" dist/unbind-linux-amd64
    install -m 755 "$target_dir/aarch64-unknown-linux-musl/release/unbind" dist/unbind-linux-arm64
}

build_macos() {
    builder=zigbuild
    if [ "$(uname -s)" = Darwin ]; then
        builder=build
    fi
    cargo "$builder" --locked --release --bin unbind --target-dir "$target_dir" \
        --target x86_64-apple-darwin --target aarch64-apple-darwin
    install -m 755 "$target_dir/x86_64-apple-darwin/release/unbind" dist/unbind-darwin-amd64
    install -m 755 "$target_dir/aarch64-apple-darwin/release/unbind" dist/unbind-darwin-arm64
}

build_windows() {
    cargo zigbuild --locked --release --bin unbind --target-dir "$target_dir" \
        --target x86_64-pc-windows-gnu --target aarch64-pc-windows-gnullvm
    install -m 755 "$target_dir/x86_64-pc-windows-gnu/release/unbind.exe" dist/unbind-windows-amd64.exe
    install -m 755 "$target_dir/aarch64-pc-windows-gnullvm/release/unbind.exe" dist/unbind-windows-arm64.exe
}

case "${1:-all}" in
    linux) build_linux ;;
    macos) build_macos ;;
    windows) build_windows ;;
    all)
        build_linux
        build_macos
        build_windows
        ;;
    *)
        printf 'Usage: %s [all|linux|macos|windows]\n' "$0" >&2
        exit 2
        ;;
esac

cd dist
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum unbind-* > SHA256SUMS
else
    shasum -a 256 unbind-* > SHA256SUMS
fi
