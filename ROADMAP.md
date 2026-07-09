# Roadmap

Design lives in [SPEC.md](./SPEC.md). This is the build order.

## Track A — on-device runtime (`runtime/`)

Vertical slices; each one runs on the AVD before the next starts.

- **M1 — bare pipeline.** ✅ **Done.** Attach to `binder:binder_transaction`, push
  `{src_pid, dst_pid, code, flags, size}` through the ring buffer, print to
  console. Verified live on an arm64 AVD — captures real binder traffic with
  correct pids/code/flags and oneway detection.
- **M2 — process names.** ✅ **Done.** Resolve `/proc/<pid>/cmdline` with a
  pid→name cache; emit `name (pid) -> name (pid)`.
- **M3 — interface descriptor + size.** ✅ **Done.** A kprobe on
  `binder_transaction()` reads `data_size` and the parcel buffer; the consumer
  UTF-16-decodes the interface descriptor (validated by the `'SYST'` token magic).
  Replies show `<reply>`, HIDL/hwbinder & special transactions show `<non-aidl>`.
  Verified live on the AVD (automotive AIDL + a HIDL bluetooth call).
- **M4 — in-kernel filter.** ✅ The probe uses the full zero-padded UTF-16LE descriptor
  as the key into a `WANTED` BPF map (collision-free, so no in-probe hashing), gated by a
  1-element `FILTER_ON` flag map (runtime-toggleable for the control app). Non-matching
  transactions are dropped in the tracepoint **before** the ring buffer. Driven by
  `--iface <name>` (repeatable, comma-separated). Verified live on the AVD: exact match
  (filtering `IVehicle` does not leak `IVehicleCallback`); tokenless/special transactions
  drop while a filter is active.
- **M5 — errors + sinks + CLI.** In progress.
  - ✅ Console sink with wall-clock timestamp.
  - ✅ Logcat sink (`--sink console|logcat|both|none`), tag `bindfetto` + `BINDFETTO` marker.
  - ✅ File / JSONL sink (`--jsonl <path>`, composes with any `--sink`; one JSON object
    per transaction). Verified live on the AVD (671 records, all valid JSON).
  - ✅ DLT server (`--dlt-serve [port]`, default 3490): bindfetto is itself the DLT
    endpoint — streams each transaction as a verbose DLT message over TCP, so DLT Viewer
    connects as a TCP ECU and shows them live with no libdlt and no dlt-daemon. Wire
    format verified against DLT Viewer's `qdlt` parser (synthetic + a real on-device
    streamed message); server verified live on the AVD.
  - ⏳ Second attach point for `BR_FAILED_REPLY`/`BR_DEAD_REPLY` (toggleable).
  - ✅ Interface filter CLI (`--iface`) — wired to the M4 in-kernel filter above.
  - ⏳ Full CLI (`--include-replies`, error toggle).

## Track B — offline decode

- **B1 — catalog builder** (`catalog/`, Python) ✅: `bindfetto_catalog.py` turns AIDL
  (a file, a recursed folder, or an http(s) URL) → JSON catalog, numbering methods by
  declaration order from `FIRST_CALL_TRANSACTION` and honoring explicit `= N`; skips
  consts/nested types; strips comments+annotations. Stdlib-only, unit-tested, and
  verified end-to-end (generated catalog → Rust decoder) and against a live AOSP
  `.aidl` URL.
- **B2 — shared decoder core + `bindfetto-decode` CLI** (`decode/`, Rust): line
  parse + catalog lookup → method name. In progress.
  - ✅ Core crate: `Catalog`/`Decoder`, prefix-agnostic `decode_line` rewrite,
    structured `Record`/`Label` parse, special-transaction table, unit tests.
  - ✅ `bindfetto-decode` stdin→stdout / file CLI.
  - ✅ C ABI (`decode/src/ffi.rs` + `decode/include/bindfetto_decode.h`,
    staticlib/cdylib crate types) for native embedders; verified with a C smoke test.
  - ✅ WASM: core builds for `wasm32-unknown-unknown`; `plugins/vscode/wasm/` re-exports
    the decoder ABI + a byte allocator. All expected symbols exported.
- **B3 — viewer plugins**:
  - ✅ DLT Viewer plugin (`plugins/dlt/`, C++/Qt `QDLTPluginDecoderInterface` over the
    C ABI): verified end-to-end on macOS (Qt 6.11) — loads via `QPluginLoader`,
    `decodeMsg` rewrites via the core. `loadConfig` takes a catalog file or a folder
    (merged via `QJsonObject`).
  - ✅ VS Code extension (`plugins/vscode/`, TypeScript over the WASM core): one command
    (**Decode Active Editor**) + `bindfetto.catalogPath` setting; `src/decoder.ts`
    marshals strings across the wasm boundary. `bindfetto.catalogPath` takes a catalog
    file or a folder (every *.json merged). Verified on Node 26: wasm builds/exports,
    `tsc` clean, Node smoke + compiled-decoder end-to-end decode pass.

## Track C — control app (`app/`, Kotlin)

- **C1 — control channel.** ✅ `--control [port]` (default 3491): a line-oriented TCP
  server driving the runtime live via a shared `RuntimeState`. Commands: `STATUS`;
  `START`/`STOP` (capture toggle); `SINK`; `DLT on|off`; `TRACK on|off` (interface
  discovery, off by default); `LIST`/`GET`/`SET`/`CLEAR` (in-kernel filter). Enabling
  `--control` auto-binds the DLT server. (TCP over localhost / `adb forward` was chosen
  over the SPEC's unix-socket + `SO_PEERCRED` design for testability; hardening deferred.)
- **C2 — app.** ✅ Kotlin + Jetpack Compose app under `app/`, three tabs:
  - **Control** — Connect + live STATUS, Start/Stop, sink selector, DLT toggle.
  - **Filter** — discovery enabled only while the tab is open, checkbox list, Apply/Clear
    push the in-kernel filter.
  - **Deploy** — best-effort deploy of the bundled binary via `su`, with an `adb` fallback.

  Verified end-to-end on the AVD (headless via `uiautomator dump` + `input tap`): every
  Control action round-trips through STATUS; discovery toggles with the Filter tab; Apply
  narrows the in-kernel capture; Deploy falls back to adb (no root/signature on a debug
  build). Still TODO: error-capture toggle (needs M5); verified privileged deploy.

Tracks B and C start once Track A produces stable output (≈after M3).
