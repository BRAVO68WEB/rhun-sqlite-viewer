#!/bin/sh
# Builds the extension for this machine into bin/ and runs tests/real.py headlessly against a rhun
# binary: open a database, see its table, run SQL, update, undo, redo, and save.
# usage: tests/run.sh [RHUN]
set -e
cd "$(dirname "$0")/.."
RHUN=${1:-$(command -v rhun || echo "$HOME/.local/bin/rhun")}
name=$(sed -n 's/^name *= *//p' extension.ini | head -1)
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=linux-x86_64 ;;
    Darwin-arm64) target=macos-arm64 ;;
    *) echo "no target for $(uname -s)-$(uname -m)"; exit 77 ;;
esac
cargo build --release --quiet
mkdir -p bin
cp "target/release/$name" "bin/$name-$target"
sh tests/sign.sh "bin/$name-$target"
RHUN=$RHUN python3 tests/real.py
