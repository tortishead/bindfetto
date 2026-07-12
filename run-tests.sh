#!/usr/bin/env bash
# Run every host-runnable bindfetto test suite and print a summary.
#
# Host-testable (this script): decode core (Rust), the wire contract (Rust), the
# catalog builder (Python), and the VS Code plugin (WASM smoke).
#
# NOT host-testable — verified live instead, see runtime/README.md:
#   * runtime/bindfetto-ebpf  — the eBPF probe; only the kernel verifier can accept it,
#                               and it runs on an arm64 Android device/AVD.
#   * runtime/bindfetto       — the consumer links `aya` (Linux-only) + Android liblog
#                               and reads /proc, so it only builds for the Android target.
#   * app/ (Kotlin), plugins/dlt (C++/Qt) — driven on-device / in DLT Viewer.
#
# Optional, only if the toolchains are present:
#   RUN_CROSS=1   also cross-compile the runtime for aarch64-linux-android (compile check)
#
# Exit code is non-zero if any suite that ran actually failed; suites whose tooling is
# missing are skipped, not failed.

set -u
cd "$(dirname "$0")"

pass=0 fail=0 skip=0
results=()

have() { command -v "$1" >/dev/null 2>&1; }

run() {   # run <name> <cmd...>
  local name=$1; shift
  printf '\n\033[1m== %s ==\033[0m\n' "$name"
  if "$@"; then
    results+=("PASS  $name"); pass=$((pass + 1))
  else
    results+=("FAIL  $name"); fail=$((fail + 1))
  fi
}

skip() { printf '\n\033[1m== %s ==\033[0m\n(skipped: %s)\n' "$1" "$2"; results+=("SKIP  $1"); skip=$((skip + 1)); }

# --- Rust: decode core + wire contract ------------------------------------------------
if have cargo; then
  run "decode (Rust)"        bash -c 'cd decode && cargo test --quiet'
  run "wire contract (Rust)" bash -c 'cd runtime && cargo test --quiet -p bindfetto-common'
else
  skip "decode (Rust)" "cargo not found"
  skip "wire contract (Rust)" "cargo not found"
fi

# --- Python: catalog builder ----------------------------------------------------------
if have python3; then
  run "catalog builder (Python)" bash -c 'cd catalog && python3 -m unittest discover -s tests'
else
  skip "catalog builder (Python)" "python3 not found"
fi

# --- VS Code plugin: WASM smoke -------------------------------------------------------
if have npm && rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown; then
  run "vscode plugin (WASM smoke)" bash -c 'cd plugins/vscode && npm run build:wasm --silent && npm run compile --silent && npm run smoke --silent'
else
  skip "vscode plugin (WASM smoke)" "npm and/or wasm32 target missing"
fi

# --- Optional: cross-compile the runtime (compile check, not a unit test) -------------
if [ "${RUN_CROSS:-0}" = "1" ]; then
  if have cargo && rustup target list --installed 2>/dev/null | grep -q aarch64-linux-android; then
    run "runtime cross-build (aarch64-android)" bash -c 'cd runtime && cargo build --quiet --release --target aarch64-linux-android'
  else
    skip "runtime cross-build (aarch64-android)" "android target/NDK not set up"
  fi
fi

# --- summary --------------------------------------------------------------------------
printf '\n\033[1m== summary ==\033[0m\n'
for r in "${results[@]}"; do printf '  %s\n' "$r"; done
printf '\n%d passed, %d failed, %d skipped\n' "$pass" "$fail" "$skip"
printf 'not host-testable (verify live): ebpf probe, consumer, app, dlt plugin\n'
exit $((fail > 0 ? 1 : 0))
