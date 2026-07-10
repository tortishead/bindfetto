# Roadmap

Design lives in [SPEC.md](./SPEC.md); this is the build order. Everything below is
done and verified live on an arm64 AVD — see the per-component READMEs for detail.

## Track A — on-device runtime (`runtime/`)

- **M1 — bare pipeline.** ✅ Tracepoint on `binder:binder_transaction` → ring buffer →
  console (`{src_pid, dst_pid, code, flags, size}`, oneway detection).
- **M2 — process names.** ✅ pid→name from `/proc/<pid>/cmdline` (cached).
- **M3 — interface descriptor + size.** ✅ kprobe reads `data_size` + the parcel; the
  consumer UTF-16-decodes the descriptor. Replies show `<reply>`, HIDL/special
  transactions `<non-aidl>`.
- **M4 — in-kernel filter.** ✅ Full zero-padded descriptor as the key into a `WANTED`
  BPF map, gated by a `FILTER_ON` flag; non-matching transactions drop before the ring
  buffer. Driven by `--iface` (repeatable, exact match).
- **M5 — errors + sinks + CLI.** ✅ Sinks: console, logcat (`--sink`), JSONL
  (`--jsonl`), DLT server (`--dlt-serve`). Error path: a `binder:binder_return`
  tracepoint (gated by `ERRORS_ON`) correlated per-thread via `LAST_TX`, with the
  concrete errno decoded from the kernel `failed_transaction_log`. CLI: `--iface`,
  `--errors`, `--include-replies`.

## Track B — offline decode

- **B1 — catalog builder** (`catalog/`, Python) ✅ AIDL (file / folder / URL) →
  `interface → {code → method}` JSON; declaration-order numbering from
  `FIRST_CALL_TRANSACTION`, honors explicit `= N`. Stdlib-only, unit-tested.
- **B2 — decoder core + CLI** (`decode/`, Rust) ✅ `Catalog`/`Decoder`, prefix-agnostic
  `decode_line`, the `bindfetto-decode` CLI, a C ABI (staticlib/cdylib + header), and a
  WASM build (`wasm32-unknown-unknown`).
- **B3 — viewer plugins** ✅ DLT Viewer plugin (C++/Qt over the C ABI) and VS Code
  extension (TypeScript over WASM). Both verified end-to-end.

## Track C — control app (`app/`, Kotlin)

- **C1 — control channel** ✅ `--control [port]`: a line-TCP server over a shared
  `RuntimeState` — `STATUS`, `START`/`STOP`, `SINK`, `DLT`, `ERRORS`, `TRACK`,
  `LIST`/`GET`/`SET`/`CLEAR`. Auto-binds the DLT server.
- **C2 — app** ✅ Jetpack Compose app, three tabs: **Control** (status, capture, sink,
  DLT + error toggles), **Filter** (interface discovery, search + manual add,
  apply/clear the in-kernel filter), **Deploy** (detached `su`/adb launch of the
  daemon). Verified end-to-end on the AVD.

## Open / deferred

- **Control channel hardening** — unix-socket + `SO_PEERCRED` (currently TCP/localhost
  for testability).
- **Verified privileged deploy** — needs platform signing or root; debug builds fall
  back to the adb path.
