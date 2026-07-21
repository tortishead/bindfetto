# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Bindfetto observes Android **Binder** IPC at the kernel level and emits human-readable
transaction logs. Design is in `docs/SPEC.md`; build order + current status in `docs/ROADMAP.md`
(read it first when resuming). Per-component build detail lives in each dir's `README.md`.

By design the system splits into a **fast on-device capture path** and a **rich offline
decode path** — the device emits the *raw* transaction code; method names are resolved
later against an AIDL catalog, so logs stay re-decodable against any catalog version.

## Architecture

**Runtime** (`runtime/`, Cargo workspace, Rust) — three crates over one wire contract:
- `bindfetto-common` — the `#[repr(C)]` `TxEvent`, the ring-buffer wire contract shared
  by probe and consumer (`no_std`; `user` feature adds `aya` `Pod` impls).
- `bindfetto-ebpf` — the `no_std` probe for `bpfel-unknown-none`. NOT a default
  workspace member; the consumer's `build.rs` compiles + embeds it via `aya-build`.
- `bindfetto` — the userspace consumer: loads the probe, drains the ring buffer,
  resolves pid→name from `/proc/<pid>/cmdline` (cached), emits. `src/main.rs` holds the
  sinks, `RuntimeState`, and the `control` module (line TCP server); `src/dlt_wire.rs`
  is the DLT encoder.

  Capture is gated **in-kernel** by BPF maps: a `WANTED` map keyed by the full
  zero-padded UTF-16LE descriptor (collision-free exact match, no hashing) + a
  `FILTER_ON` flag; error capture is a second `binder:binder_return` attach point gated
  by `ERRORS_ON`, correlated per-thread via `LAST_TX`, with the concrete errno recovered
  from the kernel `failed_transaction_log` by `debug_id`.

**Decode core** (`decode/`, Rust, host-built) — the plugin-agnostic core; `Decoder`/
`Catalog` do `(interface, code) → method` and `decode_line` rewrites `interface.[code:N]`
tokens in place (prefix-agnostic). Exposed three ways from one codebase: the
`bindfetto-decode` CLI, a C ABI (`ffi.rs` + header, staticlib/cdylib) for the DLT plugin,
and WASM (re-exported from `plugins/vscode/wasm/`) for the VS Code extension.

**Catalog builder** (`catalog/`, Python 3, stdlib only) — parses `.aidl` source into the
`interface → {code → method}` JSON: declaration-order numbering from
`FIRST_CALL_TRANSACTION`, honors explicit `= N`. Codes align to the AIDL you feed it —
use AIDL matching the device build.

**Plugins** (`plugins/`) — `dlt/` (C++/Qt over the C ABI, recognizes lines by the
`BINDFETTO` marker), `vscode/` (TypeScript over WASM).

**Control app** (`bindfetto-app/`, Kotlin/Compose) — the runtime runs as a **root daemon** (an app
can't grant itself BPF); the app drives it over TCP (`127.0.0.1:3491`). Start/Stop toggles
capture, it does not spawn the process. Protocol wrapper: `ControlClient.kt`. Tabs:
Control, Filter (discovery only while the tab is open), Deploy (`su`, adb fallback).

## Build & run

`aya` is Linux-only, so the consumer **only cross-compiles to Android**, never the macOS
host. Runtime needs nightly (pinned in `rust-toolchain.toml`) + `rust-src`, `bpf-linker`,
the `aarch64-linux-android` target, and the NDK linker (`.cargo/config.toml` expects
`aarch64-linux-android30-clang` on PATH).

```sh
# runtime — eBPF object is embedded via build.rs
cargo build --release --target aarch64-linux-android
adb root && adb shell setenforce 0                 # BPF load is SELinux-gated
adb push target/aarch64-linux-android/release/bindfetto /data/local/tmp/
adb shell /data/local/tmp/bindfetto                # run as root

./run-tests.sh                 # all host suites: decode + wire contract + catalog + wasm
cargo test                     # decode/  (host build; also produces the .a + C header)
cargo test -p bindfetto-common # runtime/ wire-contract layout invariants (host build)
python3 -m unittest discover -s tests -v   # catalog/
npm install && npm run build:wasm && npm run compile && npm run smoke   # plugins/vscode/
(cd bindfetto-app && ./gradlew :app:assembleDebug)   # needs JDK 17; build runtime first to bundle the binary
```

Test coverage splits by what's host-runnable: `./run-tests.sh` covers the decode core,
the `bindfetto-common` wire contract, the catalog builder, and the VS Code WASM path. The
**eBPF probe** (only the kernel verifier accepts it) and the **consumer** (`aya` is
Linux-only, links Android liblog, reads `/proc`) build only for the Android target and are
verified live on the AVD; the app and DLT plugin are driven on-device.

Runtime CLI flags: `--sink console|logcat|both|none`, `--jsonl <path>`,
`--dlt-serve [port]` (3490), `--iface <name>` (repeatable, exact match), `--errors [on|off]`,
`--parcel [on|off]` (M6 parcel capture; only honored with an active `--iface` filter),
`--parcel-max <bytes>` (per-transaction payload cap, default 256, clamped to a 30 KiB ceiling),
`--include-replies`, `--control [port]` (3491, auto-binds DLT), `--version`/`-V` (print
version + exit, no root). On a new device, confirm the tracepoint offsets in
`bindfetto-ebpf/src/main.rs` against
`/sys/kernel/tracing/events/binder/binder_transaction/format`.

## Release & versioning

Installers/scripts at the repo root: `install.sh` (user-facing — pulls the latest GitHub
release assets, OS-aware), `bump-version.sh` + `release.sh` (maintainer).

- **Versioning is lockstep** — one product version across **six** manifests:
  `runtime/Cargo.toml` (`[workspace.package]`), `decode/Cargo.toml`,
  `plugins/vscode/package.json`, `plugins/dlt/bindfettodecoderplugin.json` (compiled into
  the DLT `.so` via `Q_PLUGIN_METADATA`), and the app's `versionName` + `versionCode`.
  Change them only via `./bump-version.sh <ver>`; anything else drifts.
- **Releasing** — `./release.sh [ver] [--upload]` stages the built artifacts under
  canonical versioned asset names and publishes to the GitHub release. Its preflight
  refuses `--upload` unless all six manifests agree. `install.sh` resolves each asset by a
  stable prefix+suffix pattern (version-agnostic).
- **The DLT `.so` is per-host** — release.sh stages only the current OS's `.so` (never
  cross-labels); run it once on macOS and once on Linux to publish both. APK is
  debug-signed unless `BINDFETTO_KEYSTORE*` env vars point at a real keystore.

## Conventions

- **Keep the hot path cheap:** never add method-name resolution or catalog work to the
  on-device path — it belongs in the offline decode core.
- Filtering + error capture toggle through **BPF flag maps**, so the control channel
  flips them live without reattaching.
- Decode logic is written **once** in `decode/` and reused via three ABIs — change it
  there, not in a plugin.
- `TxEvent` in `bindfetto-common` is the probe↔consumer wire contract — changing it means
  changing both sides.
