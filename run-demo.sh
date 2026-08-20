#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/target/release/nost"

needs_build=0
if [[ ! -x "$BIN" ]]; then
    needs_build=1
elif [[ "$HERE/Cargo.toml" -nt "$BIN" || "$HERE/Cargo.lock" -nt "$BIN" ]]; then
    needs_build=1
elif find "$HERE/src" -type f -name '*.rs' -newer "$BIN" -print -quit | grep -q .; then
    needs_build=1
fi

if (( needs_build )); then
    "$HERE/build.sh"
fi

exec "$BIN" --demo "$@"
