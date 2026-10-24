#!/bin/sh
# Copies the files the extension ships into DEST, the folder rhun installs (protocol section 1):
# everything at the top of this repository except the development and build files listed below.
# The program is not copied: the caller builds it into DEST/bin. Names starting with a dot are
# never copied. A link fails: rhun refuses an archive that holds one.
# usage: sh tests/layout.sh DEST
set -e
dest=$1
[ -n "$dest" ] || { echo "usage: tests/layout.sh DEST" >&2; exit 2; }
mkdir -p "$dest"
dest=$(cd "$dest" && pwd)
cd "$(dirname "$0")/.."
for f in *; do
    case $f in
        tests | docs | AGENTS.md | CLAUDE.md | bin | dist | out | target) continue ;;
        src | build.rs | Cargo.toml | Cargo.lock | rust-toolchain | rust-toolchain.toml) continue ;;
        rustfmt.toml | clippy.toml | deny.toml) continue ;;
    esac
    cp -R "$f" "$dest/"
done
links=$(find "$dest" -type l)
if [ -n "$links" ]; then
    echo "rhun refuses an extension archive with links; copy the files instead:" >&2
    echo "$links" >&2
    exit 1
fi
