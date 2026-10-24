#!/bin/sh
# Builds a folder rhun can take with Install from Folder. The repository itself cannot: cargo's
# target/ holds hard links, and rhun refuses a folder that contains one.
# usage: sh stage.sh [DEST]
set -e
cd "$(dirname "$0")"
dest=${1:-"$HOME/.local/share/rhun-sqlite-viewer"}
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=linux-x86_64; exe= ;;
    Darwin-arm64) target=macos-arm64; exe= ;;
    Darwin-x86_64) echo "rhun's macOS build is arm64"; exit 77 ;;
    *) echo "no target for $(uname -s)-$(uname -m)"; exit 77 ;;
esac
cargo build --release --quiet
name=$(sed -n 's/^name *= *//p' extension.ini | head -1)
rm -rf "$dest"
sh tests/layout.sh "$dest"
mkdir -p "$dest/bin"
cp "target/release/$name$exe" "$dest/bin/$name-$target$exe"
sh tests/sign.sh "$dest/bin/$name-$target$exe"
# a hard link or a symlink fails Install from Folder
if find "$dest" \( -type l -o -type f -links +1 \) | grep -q .; then
    echo "the staged folder still has a link" >&2
    find "$dest" \( -type l -o -type f -links +1 \) >&2
    exit 1
fi
echo "$dest"
