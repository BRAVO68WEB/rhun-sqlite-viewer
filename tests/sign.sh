#!/bin/sh
# macOS kills a Rust binary that still carries only the linker's signature: exit 137,
# SIGKILL, "Code Signature Invalid". codesign replaces that with an ad-hoc signature it will run.
# usage: sh tests/sign.sh BINARY [BINARY...]
set -e
[ "$(uname -s)" = Darwin ] || exit 0
for bin in "$@"; do
    codesign --force --sign - "$bin"
done
