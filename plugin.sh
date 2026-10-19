#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-signal.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-signal" ;;
    *) rel="x86_64-apple-darwin/gray-signal" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-signal" ;;
    *) rel="x86_64-unknown-linux-musl/gray-signal" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-signal.exe" "$here/target/release/gray-signal" "$here/target/debug/gray-signal.exe" "$here/target/debug/gray-signal"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-signal binary not found in $here" >&2
exit 1
