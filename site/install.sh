#!/bin/sh
set -eu

fail() {
  printf 'unbind installer: %s\n' "$*" >&2
  exit 1
}

base_url=${1:-https://unbind.dev}
base_url=${base_url%/}
case "$base_url" in
  https://*) protocols='=https' ;;
  http://localhost:*|http://127.0.0.1:*|http://\[::1\]:*) protocols='=https,http' ;;
  *) fail 'Use an HTTPS download URL.' ;;
esac

case "$(uname -s)" in
  Linux) platform=linux ;;
  Darwin) platform=darwin ;;
  *) fail 'Use the PowerShell installer or a manual download for this platform.' ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=amd64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) fail 'This CPU architecture is unsupported.' ;;
esac

if [ "$platform" = darwin ] && [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
  arch=arm64
fi

asset=unbind-$platform-$arch
bin_dir=${UNBIND_BIN_DIR:-$HOME/.local/bin}
command -v curl >/dev/null 2>&1 || fail 'Install curl and run this command again.'
temp_dir=$(mktemp -d)
staged_binary=''
cleanup() {
  rm -rf "$temp_dir"
  if [ -n "$staged_binary" ]; then rm -f "$staged_binary"; fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

download() {
  curl -fsSL --proto "$protocols" --proto-redir "$protocols" "$base_url/downloads/$1" -o "$temp_dir/$1"
}

download SHA256SUMS || fail "Could not get checksums from $base_url."
expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print tolower($1); exit }' "$temp_dir/SHA256SUMS")
case "$expected" in
  ''|*[!0-9a-f]*) fail "No published build for $platform/$arch at $base_url." ;;
esac
[ "${#expected}" -eq 64 ] || fail "Invalid checksum for $asset."

download "$asset" || fail "Could not download $asset from $base_url."
if command -v sha256sum >/dev/null 2>&1; then
  checksum_output=$(sha256sum < "$temp_dir/$asset")
elif command -v shasum >/dev/null 2>&1; then
  checksum_output=$(shasum -a 256 < "$temp_dir/$asset")
else
  fail 'Install sha256sum or shasum to check the download.'
fi
actual=${checksum_output%% *}
[ "$expected" = "$actual" ] || fail "Checksum mismatch for $asset."

mkdir -p "$bin_dir"
staged_binary=$(mktemp "$bin_dir/.unbind.XXXXXXXXXX")
cp "$temp_dir/$asset" "$staged_binary"
chmod 0755 "$staged_binary"
mv -f "$staged_binary" "$bin_dir/unbind"
staged_binary=''

printf 'Installed unbind to %s/unbind\n' "$bin_dir"
case ":$PATH:" in
  *":$bin_dir:"*) printf 'Run unbind --help to get started.\n' ;;
  *) printf 'Add %s to PATH, or run %s/unbind --help.\n' "$bin_dir" "$bin_dir" ;;
esac
