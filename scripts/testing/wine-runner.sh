#!/bin/sh
# Cargo test runner for Windows binaries under Wine on a private X server:
#   CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=scripts/testing/wine-runner.sh
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
export WINEDEBUG=-all
export WINEPREFIX="$ROOT/target/wine-prefix"
mkdir -p "$WINEPREFIX"
exec xvfb-run -a -s "-screen 0 1920x1080x24" wine "$@"
