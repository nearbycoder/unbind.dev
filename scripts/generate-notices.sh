#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
directory=$(mktemp -d)
trap 'rm -rf "$directory"' EXIT
trap 'exit 1' HUP INT TERM

# Exclude Unbind itself from the third-party report without changing its manifest.
awk '1; $0 == "[package]" { print "publish = false" }' Cargo.toml > "$directory/Cargo.toml"
cp Cargo.lock "$directory/"
ln -s "$PWD/src" "$directory/src"

cargo about generate --locked --fail --manifest-path "$directory/Cargo.toml" \
    --config about.toml --output-file "$directory/third-party-notices.txt" \
    scripts/third-party-notices.hbs
mv "$directory/third-party-notices.txt" site/third-party-notices.txt
