# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Bindfetto observes Android **Binder** IPC at the kernel level and emits human-readable
transaction logs. Design is in `SPEC.md`; build order + current status in `ROADMAP.md`
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

**Control app** (`app/`, Kotlin/Compose) — the runtime runs as a **root daemon** (an app
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

cargo test                     # decode/  (host build; also produces the .a + C header)
python3 -m unittest discover -s tests -v   # catalog/
npm install && npm run build:wasm && npm run compile && npm run smoke   # plugins/vscode/
./gradlew :app:assembleDebug   # app/ (needs JDK 17; build runtime first to bundle the binary)
```

Runtime CLI flags: `--sink console|logcat|both|none`, `--jsonl <path>`,
`--dlt-serve [port]` (3490), `--iface <name>` (repeatable, exact match), `--errors [on|off]`,
`--include-replies`, `--control [port]` (3491, auto-binds DLT). On a new device, confirm the
tracepoint offsets in `bindfetto-ebpf/src/main.rs` against
`/sys/kernel/tracing/events/binder/binder_transaction/format`.

## Conventions

- **Keep the hot path cheap:** never add method-name resolution or catalog work to the
  on-device path — it belongs in the offline decode core.
- Filtering + error capture toggle through **BPF flag maps**, so the control channel
  flips them live without reattaching.
- Decode logic is written **once** in `decode/` and reused via three ABIs — change it
  there, not in a plugin.
- `TxEvent` in `bindfetto-common` is the probe↔consumer wire contract — changing it means
  changing both sides.
