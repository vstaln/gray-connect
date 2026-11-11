#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-connect.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-connect" ;;
    *) rel="x86_64-apple-darwin/gray-connect" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-connect" ;;
    *) rel="x86_64-unknown-linux-musl/gray-connect" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-connect.exe" "$here/target/release/gray-connect" "$here/target/debug/gray-connect.exe" "$here/target/debug/gray-connect"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-connect binary not found in $here" >&2
exit 1
