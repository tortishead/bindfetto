# BINDFETTO — Specification

> A binder log viewer for Android.

## What it is

Bindfetto observes Android **Binder** IPC traffic at the kernel level and surfaces
it as human-readable transaction logs. Instead of guessing at cross-process calls,
a developer can see the flow of Binder transactions live.

> **Status:** clean-room rewrite for a public GitHub project. The core concept —
> including kernel-level interface-descriptor extraction — is already proven by a
> working internal PoC with the same setup. This spec is about a well-structured
> re-implementation, not de-risking feasibility.

## How it works

The system splits into a **runtime capture path** (on-device, fast, no name
lookups) and an **offline decode path** (in a log viewer, using a precompiled
AIDL catalog). Keeping method-name resolution offline keeps the hot path cheap
and lets logs be re-decoded later against different catalogs.

### Runtime (on device)

1. **Kernel-level capture via eBPF.** An `aya` eBPF program attaches to the
   kernel's `binder_transaction` path (tracepoint/kprobe) and pushes a compact
   record for every transaction into a **ring buffer**.
2. **Userspace consumer (Rust binary).** Consumes the ring buffer and:
   - **Resolves source & target process names** (from pid → process name; see
     [Process name resolution](#process-name-resolution)).
   - **Emits the log to a configurable sink** (see [Output sinks](#output-sinks)).
   Each emitted record carries: source/target (name + pid), the **interface
   descriptor token**, the raw **transaction code**, and the **parcel size** —
   but *not* a decoded method name.

### Captured event types

- **Transactions** — source → target, interface, code, parcel size, and the
  **oneway/sync** flag (`TF_ONE_WAY`).
- **Transaction errors** — `BR_FAILED_REPLY`, `BR_DEAD_REPLY` and `BR_FROZEN_REPLY`,
  logged with a **human-readable error code** (and, when recoverable, the concrete
  errno/reason) plus the failing source → target, interface and method. These come from
  the binder **return/error path**, which requires a **second attach point** in the
  probe (distinct from the `binder_transaction` entry hook). Error capture is
  **toggleable** and off unless enabled via the control app or the CLI.

> **Deferred:** oneway (async) spam detection is out of scope for now.

### Output sinks

The consumer writes each formatted log line to one of (`--sink console|logcat|both|none`):

- **logcat** — appears alongside normal logs under tag **`bindfetto`**, and each
  message is prefixed with the **`BINDFETTO`** marker. Either is enough for the
  decoder to select bindfetto's lines: filter by tag (`logcat -s bindfetto`), or
  match `BINDFETTO` in a merged/DLT log where the tag may be flattened. On automotive
  targets the OEM already pipes logcat into DLT, so **no logcat→DLT bridge is ours to
  build**.
- **console** (stdout) — for local/interactive use; adds a wall-clock timestamp.
- **local file (JSONL)** — `--jsonl <path>` writes one structured JSON object per
  transaction for offline capture and decoding. Composes with any `--sink` (use
  `--sink none` for a file-only capture).
- **DLT server** — `--dlt-serve [port]` (default 3490) makes bindfetto itself the DLT
  endpoint: it streams each transaction as a verbose DLT message over TCP, so DLT Viewer
  connects as a TCP ECU and shows them **live** — no libdlt and no dlt-daemon. This is
  the fallback for targets where the OEM does *not* bridge logcat into DLT; where the
  bridge exists, the logcat sink already reaches DLT.

### Process name resolution

The probe emits **pids** (sender tgid, target `to_proc`); the consumer resolves
names in userspace against `/proc`, with a **pid→name cache** since a pid's name
is stable for its lifetime:

1. `/proc/<pid>/cmdline` — the **full Android process name** (e.g.
   `com.example.app`, `system_server`). Preferred.
2. `/proc/<pid>/comm` — fallback; note it is truncated to 15 chars, so long
   package names are cut off.
3. For a process that already exited before the consumer reads the event, fall
   back to the sender's `comm` captured in-kernel via `bpf_get_current_comm()`
   (sender only), otherwise render `pid:<n>`.

Only pids are read in-kernel (no task-struct walking), keeping the probe simple and
kernel-version portable. Pid reuse is a minor risk; optionally guarded later by
checking process `starttime` from `/proc/<pid>/stat`.

### Control surfaces

The on-device runtime is controllable two ways, exposing the same operations:

- **CLI** — the binary accepts command-line parameters, so it can be driven directly
  from a shell (adb/root) with no app: start/stop, output sink, interface filter,
  and error-capture toggle.
- **Control app (Android GUI)** — an operator-friendly front end that talks to the
  consumer over a **control channel**: a line-oriented **TCP** server (`--control
  [port]`, default 3491; `adb forward` from a host). Unix-socket + `SO_PEERCRED`
  hardening is deferred (see Technical decisions).

Operations exposed by both:

0. **Install/deploy the binary** *(app only)* — if the app holds **signature-level
   permission** (signed with the platform key), it places the binary in a privileged
   location and launches it, no adb/root shell needed. Without signature permission,
   deployment falls back to the adb/CLI path.
1. **Start/stop collection** — control the capture lifecycle.
2. **Filter interface names** — push an interface filter so only matching interfaces
   are captured/emitted. Applied **in-kernel** via a `WANTED` BPF map keyed by the full
   (zero-padded) descriptor, dropping non-matching events before the ring buffer.
3. **Toggle error capture** — enable/disable the `BR_FAILED_REPLY`/`BR_DEAD_REPLY`
   attach point at runtime.

### Offline (in a viewer)

3. **Method-name decoding — the highlight.** A viewer plugin
   (**DLT Viewer** and/or **VS Code**) reads the captured logs and, using a
   **precompiled AIDL catalog**, maps `(interface, transaction code)` →
   human-readable **method name**. Because decoding is a lookup against a catalog
   rather than runtime introspection, the same raw logs can be decoded against
   any catalog version after the fact.

## Log line format

Shape: `source (pid) -> target (pid): interface.method, parcel_size`. The runtime
doesn't know the method name, so it emits the raw transaction code in that slot; the
viewer replaces it with the method name during decoding. Format may iterate.

Raw (runtime emits; method not yet decoded):

```
com.example.app (1234) -> system_server (5678): android.app.IActivityManager.[code:7], 512B
```

After viewer decoding via the AIDL catalog:

```
com.example.app (1234) -> system_server (5678): android.app.IActivityManager.startActivity, 512B
```

## Components

| Component | Language | Role |
|---|---|---|
| **eBPF probe** | Rust (`aya`) | Kernel-side capture of transactions and (toggleable) errors; copies raw descriptor bytes and uses the full descriptor as the key for in-kernel filtering → ring buffer. |
| **Userspace consumer** | Rust | Drains ring buffer, resolves process names, applies interface filter, emits to sink. Driven by CLI args or the control app. |
| **Control app** | Kotlin (Android) | GUI to deploy the binary, start/stop collection, set interface filters, toggle error capture. |
| **AIDL catalog builder** | Python | Builds a JSON catalog mapping `(interface, code)` → method name by parsing `.aidl` source (a file, a recursed folder, or an http(s) URL); no `aidl` compiler needed. |
| **DLT Viewer plugin** | C++/Qt | Decodes captured logs against the catalog inside DLT Viewer (a thin `QDLTPluginDecoderInterface` shell over the Rust core's C ABI). |
| **VS Code plugin** | TypeScript | Same decoding, as a VS Code extension. |

### AIDL catalog (JSON)

A Python script takes AIDL (a file, a recursed folder, or an http(s) URL) and emits a
JSON catalog. It parses the `.aidl` **source directly** — numbering methods per
interface in declaration order from `IBinder.FIRST_CALL_TRANSACTION` (1) and honoring
explicit `= N` ids — so no `aidl` compiler or generated stubs are required. The catalog
is the shared contract between the (dumb) runtime logs and the (smart) viewer decoders:

```
{
  "android.app.IActivityManager": { "1": "getTasks", "7": "startActivity", ... },
  ...
}
```
Transaction codes are assigned per-interface starting at
`IBinder.FIRST_CALL_TRANSACTION` (1) in AIDL declaration order.

## Technical decisions

- **Language & eBPF toolchain: Rust with [`aya`](https://aya-rs.dev).** The kernel-side
  probe *and* the userspace loader are both written in Rust — no C. Chosen for
  readability (one language end-to-end) with no meaningful performance cost: the
  probe runs as verified BPF bytecode in-kernel regardless of source language, and
  Rust userspace is native-code, GC-free, and C-equivalent. Trade-off accepted:
  thinner Android-specific precedent than the AOSP libbpf+C path, de-risked with an
  early proof-of-concept.
- **Deployment (initial): standalone binary pushed via `adb` to a rooted device.**
  Cross-compiled to `aarch64-linux-android` and run as root. Assumes the target
  kernel has BPF + BTF enabled. Magisk/KernelSU packaging can come later.
- **Method-name resolution is offline, catalog-based.** The runtime never decodes
  method names; it emits the interface token + raw transaction code. Viewer plugins
  decode against a precompiled AIDL→JSON catalog. Keeps the hot path cheap and makes
  logs re-decodable against any catalog version.
- **Interface identity on the wire: the Binder interface descriptor string** (e.g.
  `android.app.IActivityManager`), extracted from the transaction. Not in the
  tracepoint — read from the transaction data buffer / binder internals. **Proven in
  the prior PoC;** the rewrite ports that approach.
- **Hybrid parcel handling.** The probe copies the raw (bounded) descriptor bytes out
  of the (soon-freed) parcel into the event and, for filtering, matches those bytes
  directly against the `WANTED` map; **interpretation (UTF-16→UTF-8 decode, formatting)
  happens in Rust userspace**, where it is unit-testable with byte fixtures. The header
  offset is the only version-specific constant the probe needs.
- **In-kernel interface filter via the full descriptor.** The interface filter looks
  the in-kernel descriptor up in a `WANTED` BPF map keyed by the full zero-padded
  UTF-16LE descriptor (collision-free, so no in-probe hashing), gated by a 1-element
  `FILTER_ON` flag map; non-matching events drop **before** the ring buffer, keeping
  the high-volume path cheap. (New in the rewrite; not in the PoC.)
- **AIDL catalog input: all `.aidl` files under a folder passed as a parameter.**
  The Python builder recurses a given directory. *Caveat:* to keep transaction codes
  aligned, that folder must be the AIDL that matches the device build.
- **Control channel: line-oriented TCP** (localhost / `adb forward`) between control
  app and consumer — chosen over the original unix-socket design for testability. The
  command set: `STATUS`, `START`/`STOP`, `SINK`, `DLT`, `ERRORS`, `TRACK`,
  `LIST`/`GET`/`SET`/`CLEAR`. **Deferred hardening:** unix domain socket with
  peer-credential (`SO_PEERCRED`) auth (needs an SELinux rule for the app's domain to
  reach the daemon socket).
- **Shared decoder core + thin plugins.** Catalog lookup + line parsing live in one
  plugin-agnostic core, exposed as a `bindfetto-decode` CLI; DLT Viewer and VS Code
  plugins are thin adapters over it. One viewer path shipped first.
- **Catalog built by parsing `.aidl` source.** The Python builder reads `.aidl`
  directly (stdlib-only, no `aidl` compiler): it numbers methods per interface in
  declaration order from `FIRST_CALL_TRANSACTION` and honors explicit `= N` ids,
  skipping consts/nested types and stripping comments/annotations. Special
  transactions (INTERFACE/DUMP/PING/SYSPROPS/SHELL_CMD) are resolved by the decoder
  itself, not the catalog; native (non-AIDL) interfaces are out of coverage. *Caveat:*
  feed the AIDL that matches the device build so codes align.
- **Audience & platform reality: this is a platform-developer tool.** Loading eBPF is
  gated by SELinux, not Android permissions — in practice it needs userdebug/eng
  builds or custom SELinux policy. A signed app on a `user` build generally cannot
  call `bpf()`. The docs state this plainly.
- **Initial dev target: Android emulator (AVD), arm64 system image.** Runs natively
  on the Apple-silicon dev host; recent AVD kernels (5.10/5.15) provide **BTF** and
  **`RingBuf`**. Requires `adb root` and `setenforce 0` to load BPF. Consumer
  cross-compiles to `aarch64-linux-android`. CO-RE portability across other kernels
  is deferred.

## Rewrite design notes

Concept is proven; these are structure choices to make the public rewrite clean,
testable, and maintainable. Adopt/reject per preference.

- **Why eBPF over Perfetto/atrace:** ftrace binder tracepoints already expose
  source/target/size/sync, but **cannot read the parcel** — so they can't recover the
  interface descriptor or method. Parcel access is what justifies the eBPF path;
  worth stating up front.
- **Structured machine sink:** offer **JSONL** for the file sink the decoders consume
  (robust), keep pretty text for logcat/console (human).

## Open questions

Resolved during the build (kept for the record):

- **Transaction-error attach point** → a `binder:binder_return` tracepoint watching the
  return `cmd`, gated by `ERRORS_ON` and correlated per-thread via `LAST_TX`. Also
  surfaces `BR_FROZEN_REPLY`, and decodes the concrete errno from the kernel
  `failed_transaction_log`.
- **Control command protocol** → a line-oriented TCP protocol (see Control surfaces).
- **Binary deployment without signature permission** → the binary lives in
  `/data/local/tmp`; the app tries `su`, else prints the `adb push` + run fallback.

Still open:

- **CO-RE portability** across kernels — deferred; the initial dev target is fixed.
- **Control channel hardening** — unix socket + `SO_PEERCRED` (see Technical decisions).

## M6 — parcel payload capture

The device today reads the parcel only far enough to recover the interface descriptor
(M3). The method **arguments** — the rest of the parcel — are never captured. M6 adds
raw parcel-byte capture on the hot path and full argument decoding offline.

**Core principle — the device stays dumb.** The probe copies *raw parcel bytes* and
nothing more. All structure parsing — header skip, argument marshalling, rendering —
happens offline in `decode/`, exactly as `(interface, code) → method` already does.
Logs stay re-decodable against any future Parcel-layout logic or catalog version.

### On-device (`runtime/`)

- **Gated behind a `PARCEL_ON` flag map** (same pattern as `ERRORS_ON`): off = exact
  current cost, zero payload reads. Flipped live over the control channel.
- **`PARCEL_ON` is only settable while the interface filter is active** (`FILTER_ON`
  set + `WANTED` non-empty). Full-device payload capture would multiply ring traffic
  and observer effect; requiring the filter bounds capture to the few interfaces the
  operator selected. Enforced in userspace/control, **not** the probe — the hot path
  just reads the flag. (A raw "≤ N interfaces" cap is a poor proxy — one chatty
  interface outruns ten quiet ones; filter-required is the real guard. An optional
  probe-side bytes/sec budget → `parcel_len = 0` backpressure is a later refinement.)
- **Configurable cap, runtime-tunable.** Default 256 B (`PARCEL_CAP_DEFAULT`) keeps
  casual capture cheap; the operator raises it with `--parcel-max <bytes>` /
  `PARCEL max <n>` (live, over the control channel) to inspect a big parcel, accepting
  the ring/CPU cost. Clamped to a **compile-time ceiling** `PARCEL_CEILING` (30 KiB): the
  verifier needs a constant bound, and the kernel caps a per-CPU map value at
  `PCPU_MIN_UNIT_SIZE` (32 KiB), which the scratch buffer must fit under. (64 KiB isn't
  reachable this way — it would need a fixed 64 KiB ring record, wasteful, or multi-record
  chunking.) Real arguments (handles, ints, short strings) fit the default; large blobs
  (Bitmap, buffers) pass as fds/binders, not inline.
- **Never touch the 512-byte BPF stack with payload.** `Stash` grows only by
  `buf_ptr: u64` (kept first, with an explicit `_pad`, so the map-inserted struct has no
  uninitialized padding — the verifier rejects a map read that touches it). Capture happens
  in the tracepoint: it stages a `TxRecord` in a **per-CPU scratch map** (the payload can
  reach `PARCEL_CEILING`, too big for the stack), `bpf_probe_read_user_buf`s from `buf_ptr`
  into it, then writes a **variable-length** record to the ring via `bpf_ringbuf_output` —
  only `header + parcel_len` bytes. Payload captured from **parcel offset 0** (head + body
  up to the runtime cap), so the probe needs no understanding of the strict-mode /
  interface-token header — the offline reader reconstructs descriptor → header → args.
- **Two record shapes on one ring**, distinguished by item length so the no-parcel path
  stays byte-identical to today: a bare `TxEvent` (reserve/submit, zero-copy) when parcel
  capture is off/skipped, or a variable-length `TxRecord { ev, parcel_len, parcel[..] }`
  when a payload is captured. `TxEvent` itself is unchanged — the header contract holds.
- **Ring footprint** grows only by the *actually captured* bytes per event (variable
  length), so a large cap costs ring space only when a large parcel really flows; small
  parcels stay cheap regardless of the cap. `EVENTS` bumped 256 KB → 1 MB. Cost is memory,
  not CPU (one extra staging copy, on the parcel path only).
- **Emit raw only.** No hex-encoding or rendering on device — the consumer appends the
  captured bytes to the line (`parcel=<captured>/<total>:<hex>`) / `parcel`+`parcel_len`
  in JSONL.

### Offline (`catalog/` + `decode/`)

- **Catalog v2** — extend each entry from a bare name to name + argument types,
  back-compatible (decoder accepts both the v1 string and the v2 object):
  ```json
  "1": { "name": "acquireWakeLock",
         "args": [{"name":"lock","type":"IBinder"},
                  {"name":"flags","type":"int"},
                  {"name":"tag","type":"String"}] }
  ```
  The builder already isolates the `(...)` param list; add type extraction (primitives,
  `String`, `IBinder`, arrays `T[]`, `List<T>`, `in/out/inout`, `@nullable`). Decoding a
  parcelable's *fields* is deep and version-sensitive — first cut renders parcelables as
  raw/hex.
- **Parcel reader** (`decode/parcel.rs`, written once, reused via CLI / C-ABI / WASM) —
  a `ParcelReader` over `&[u8]` following Binder marshalling: LE, 4-byte alignment;
  int32/int64/float/double; `String16` (int32 len, `-1` = null, `(len+1)·2` bytes
  UTF-16LE, padded to 4); arrays (int32 count + elements); `IBinder`/FD rendered as
  `<binder>`/`<fd>`. It skips the parcel header (strict-mode + interface token) with the
  same rules, since the device captured from offset 0. **Truncation-aware**: the payload
  is capped/partial by design, so it stops at buffer end and marks `…(truncated)`.
- **Render** — `decode_line` already rewrites `.[code:N]`; extend it to also consume a
  trailing `parcel=<hex>` token, decode the args, and emit e.g.
  `IPowerManager.acquireWakeLock(lock=<binder>, flags=1, tag="scr"…), 512B [parcel 64/512B]`.
  Stays prefix-agnostic (bare / console / DLT / logcat).

### Biggest risk

The argument-start offset — the header `writeInterfaceToken` writes (strict-mode policy,
work-source, vendor header) is non-trivial and version-dependent. Capturing from offset 0
pushes that entirely offline, where it's fixable without a device reflash. Unit-test the
reader against real captured parcels before trusting argument output.

## Non-goals (initial)

- Not a passive analyzer only — but bindfetto does **not modify or block**
  transactions; it observes.
- Not a full profiler or tracing suite; scope is Binder IPC visibility.
- No cross-device aggregation/backend; capture and decode are local.
- Oneway (async) spam detection is deferred, not in the initial scope.
