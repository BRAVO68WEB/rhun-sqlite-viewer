#!/bin/sh
# Builds the extension, lays it out as rhun installs it, and runs the conformance checker
# (rhun-editor/extension-protocol, conformance/ext) against it. usage: tests/check.sh
# RHUN_EXT_CHECK=path runs a checker you built instead of `go run …@latest`.
set -e
cd "$(dirname "$0")/.."
name=$(sed -n 's/^name *= *//p' extension.ini | head -1)
owner=$(sed -n 's/^owner *= *//p' extension.ini | head -1)
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=linux-x86_64 ;;
    Darwin-arm64) target=macos-arm64 ;;
    *) echo "no target for $(uname -s)-$(uname -m)"; exit 77 ;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
dir=$work/$owner/$name
mkdir -p "$dir/bin"
sh tests/layout.sh "$dir"
cargo build --release --quiet
cp "target/release/$name" "$dir/bin/$name-$target"
sh tests/sign.sh "$dir/bin/$name-$target"
if [ -n "$RHUN_EXT_CHECK" ]; then
    "$RHUN_EXT_CHECK" "$dir"
else
    go run github.com/rhun-editor/extension-protocol/conformance/ext@latest "$dir"
fi
