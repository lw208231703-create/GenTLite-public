#!/usr/bin/env bash
# GenTLite M1 consumer acceptance, C++ GenApi layer.
#
# Builds gentl_gige.dll (cargo), builds the GenApi consumer test (CMake,
# Visual Studio 18 2026, x64), starts sim_server on loopback, and runs
# test_genapi.exe against the DLL.
#
# Environment overrides:
#   GENAPI_ROOT  GenICam SDK root        (REQUIRED: no default — pass yours)
#   GENERATOR    CMake generator         (default "Visual Studio 18 2026")
#   BUILD_DIR    CMake build directory   (default <repo>/consumer-test/build)
#
# Git Bash on Windows. Exit 0 = PASS.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "$GENAPI_ROOT" ]; then
    echo "GENAPI_ROOT is not set. Pass the path to your EMVA GenICam SDK," >&2
    echo "e.g.  GENAPI_ROOT='C:/Program Files/GenICam' bash $0" >&2
    exit 2
fi
GENERATOR="${GENERATOR:-Visual Studio 18 2026}"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/consumer-test/build}"
CONFIG="${CONFIG:-Debug}"

DLL="$REPO_ROOT/target/debug/gentl_gige.dll"
SIM_BIN="$REPO_ROOT/target/debug/sim_server.exe"

echo "==> cargo build (gentl-gige cdylib + sim_server)"
cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p gentl-gige -p gentl-simulator
[ -f "$DLL" ] || { echo "missing $DLL"; exit 1; }
[ -f "$SIM_BIN" ] || { echo "missing $SIM_BIN"; exit 1; }

echo "==> cmake configure ($GENERATOR, x64)"
cmake -S "$REPO_ROOT/consumer-test" -B "$BUILD_DIR" -G "$GENERATOR" -A x64 \
    -DGENAPI_ROOT="$GENAPI_ROOT"

echo "==> cmake build ($CONFIG)"
cmake --build "$BUILD_DIR" --config "$CONFIG"

# --- sim camera on loopback -----------------------------------------------
SIM_LOG="$BUILD_DIR/sim_server.log"
: > "$SIM_LOG"
"$SIM_BIN" --port 0 >"$SIM_LOG" 2>&1 &
SIM_BG_PID=$!

cleanup() {
    # bash job control cannot kill native children reliably here; taskkill
    # by image name does. sim_server.exe is unique to this harness.
    taskkill //F //IM sim_server.exe >/dev/null 2>&1 || true
}
trap cleanup EXIT

PORT=""
for _ in $(seq 1 100); do
    PORT=$(grep -aoE 'SIM_PORT [0-9]+' "$SIM_LOG" 2>/dev/null | head -1 | awk '{print $2}')
    [ -n "$PORT" ] && break
    sleep 0.1
done
if [ -z "$PORT" ]; then
    echo "sim_server did not report a port; log:"
    cat "$SIM_LOG"
    exit 1
fi
echo "==> sim_server on UDP $PORT (pid $SIM_BG_PID)"

# Producer-side hooks for the loopback camera (notes/OPEN-QUESTIONS Q11).
export GENTLITE_GVCP_PORT="$PORT"
export GENTLITE_DISCOVERY_TARGETS="127.0.0.1:$PORT"
# GenApi runtime DLLs (GenApi_MD_VC141_v3_5.dll and its deps) live here.
# cygpath keeps MSYS from mangling the drive letter when exporting PATH.
export PATH="$(cygpath -u "$GENAPI_ROOT/bin/Win64_x64"):$PATH"

echo "==> test_genapi.exe $DLL"
"$BUILD_DIR/$CONFIG/test_genapi.exe" "$(cygpath -w "$DLL")"
echo "==> CONSUMER ACCEPTANCE PASSED"
