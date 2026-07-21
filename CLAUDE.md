# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Bindfetto watch Android **Binder** IPC at kernel level, emit human-readable transaction logs. Design in `docs/SPEC.md`; build order + current status in `docs/ROADMAP.md` (read first when resuming). Per-component build detail in each dir's `README.md`.

By design system split into **fast on-device capture path** and **rich offline decode path** — device emit *raw* transaction code; method names resolved later against AIDL catalog, so logs stay re-decodable against any catalog version.

## Architecture

**Runtime** (`runtime/`, Cargo workspace, Rust) — three crates over one wire contract:
- `bindfetto-common` — the `#[repr(C)]` `TxEvent`, ring-buffer wire contract shared by probe and consumer (`no_std`; `user` feature add `aya` `Pod` impls).
- `bindfetto-ebpf` — `no_std` probe for `bpfel-unknown-none`. NOT default workspace member; consumer's `build.rs` compile + embed it via `aya-build`.
- `bindfetto` — userspace consumer: load probe, drain ring buffer, resolve pid→name from `/proc/<pid>/cmdline` (cached), emit. `src/main.rs` hold sinks, `RuntimeState`, and `control` module (line TCP server); `src/dlt_wire.rs` is DLT encoder.

  Capture gated **in-kernel** by BPF maps: `WANTED` map keyed by full zero-padded UTF-16LE descriptor (collision-free exact match, no hashing) + `FILTER_ON` flag; error capture is second `binder:binder_return` attach point gated by `ERRORS_ON`, correlated per-thread via `LAST_TX`, concrete errno recovered from kernel `failed_transaction_log` by `debug_id`.

**Decode core** (`decode/`, Rust, host-built) — plugin-agnostic core; `Decoder`/`Catalog` do `(interface, code) → method` and `decode_line` rewrite `interface.[code:N]` tokens in place (prefix-agnostic). Exposed three ways from one codebase: `bindfetto-decode` CLI, C ABI (`ffi.rs` + header, staticlib/cdylib) for DLT plugin, and WASM (re-exported from `plugins/vscode/wasm/`) for VS Code extension.

**Catalog builder** (`catalog/`, Python 3, stdlib only) — parse `.aidl` source into `interface → {code → method}` JSON: declaration-order numbering from `FIRST_CALL_TRANSACTION`, honor explicit `= N`. Codes align to AIDL you feed it — use AIDL matching device build.

**Plugins** (`plugins/`) — `dlt/` (C++/Qt over C ABI, recognize lines by `BINDFETTO` marker), `vscode/` (TypeScript over WASM).

**Control app** (`bindfetto-app/`, Kotlin/Compose) — runtime run as **root daemon** (app cannot grant itself BPF); app drive it over TCP (`127.0.0.1:3491`). Start/Stop toggle capture, not spawn process. Protocol wrapper: `ControlClient.kt`. Tabs: Control, Filter (discovery only while tab open), Deploy (`su`, adb fallback).

## Build & run

`aya` Linux-only, so consumer **only cross-compile to Android**, never macOS host. Runtime need nightly (pinned in `rust-toolchain.toml`) + `rust-src`, `bpf-linker`, `aarch64-linux-android` target, and NDK linker (`.cargo/config.toml` expect `aarch64-linux-android30-clang` on PATH).

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

Test coverage split by what host-runnable: `./run-tests.sh` cover decode core, `bindfetto-common` wire contract, catalog builder, and VS Code WASM path. **eBPF probe** (only kernel verifier accept it) and **consumer** (`aya` Linux-only, link Android liblog, read `/proc`) build only for Android target, verified live on AVD; app and DLT plugin driven on-device.

Runtime CLI flags: `--sink console|logcat|both|none`, `--jsonl <path>`, `--dlt-serve [port]` (3490), `--iface <name>` (repeatable, exact match), `--errors [on|off]`, `--parcel [on|off]` (M6 parcel capture; only honored with active `--iface` filter), `--parcel-max <bytes>` (per-transaction payload cap, default 256, clamped to 30 KiB ceiling), `--include-replies`, `--control [port]` (3491, auto-binds DLT), `--version`/`-V` (print version + exit, no root). On new device, confirm tracepoint offsets in `bindfetto-ebpf/src/main.rs` against `/sys/kernel/tracing/events/binder/binder_transaction/format`.

## Release & versioning

Installers/scripts at repo root: `install.sh` (user-facing — pull latest GitHub release assets, OS-aware), `bump-version.sh` + `release.sh` (maintainer).

- **Versioning lockstep** — one product version across **six** manifests: `runtime/Cargo.toml` (`[workspace.package]`), `decode/Cargo.toml`, `plugins/vscode/package.json`, `plugins/dlt/bindfettodecoderplugin.json` (compiled into DLT `.so` via `Q_PLUGIN_METADATA`), and app's `versionName` + `versionCode`. Change only via `./bump-version.sh <ver>`; anything else drift.
- **Releasing** — `./release.sh [ver] [--upload]` stage built artifacts under canonical versioned asset names, publish to GitHub release. Preflight refuse `--upload` unless all six manifests agree. `install.sh` resolve each asset by stable prefix+suffix pattern (version-agnostic).
- **DLT `.so` per-host** — release.sh stage only current OS's `.so` (never cross-labels); run once on macOS and once on Linux to publish both. APK debug-signed unless `BINDFETTO_KEYSTORE*` env vars point at real keystore.

## Conventions

- **Keep hot path cheap:** never add method-name resolution or catalog work to on-device path — belong in offline decode core.
- Filtering + error capture toggle through **BPF flag maps**, so control channel flip them live without reattaching.
- Decode logic written **once** in `decode/`, reused via three ABIs — change there, not in plugin.
- `TxEvent` in `bindfetto-common` is probe↔consumer wire contract — change it mean change both sides.