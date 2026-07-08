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
- **Transaction errors** — only `BR_FAILED_REPLY` and `BR_DEAD_REPLY`, logged with a
  **human-readable error code** and the failing source → target. These come from the
  binder **return/error path**, which requires a **second attach point** in the
  probe (distinct from the `binder_transaction` entry hook). Error capture is
  **toggleable** and off unless enabled via the control app or the CLI.

> **Deferred:** oneway (async) spam detection is out of scope for now.

### Output sinks

The consumer writes each formatted log line to one of (`--sink console|logcat|both`):

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
  consumer over a **control channel** (mechanism TBD — e.g. a unix domain socket or
  local service).

Operations exposed by both:

0. **Install/deploy the binary** *(app only)* — if the app holds **signature-level
   permission** (signed with the platform key), it places the binary in a privileged
   location and launches it, no adb/root shell needed. Without signature permission,
   deployment falls back to the adb/CLI path.
1. **Start/stop collection** — control the capture lifecycle.
2. **Filter interface names** — push an interface filter so only matching interfaces
   are captured/emitted. Applied **in-kernel** via a BPF map of wanted descriptor
   hashes, dropping non-matching events before the ring buffer.
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
| **eBPF probe** | Rust (`aya`) | Kernel-side capture of transactions and (toggleable) errors; copies raw descriptor bytes, hashes them for in-kernel filtering → ring buffer. |
| **Userspace consumer** | Rust | Drains ring buffer, resolves process names, applies interface filter, emits to sink. Driven by CLI args or the control app. |
| **Control app** | Kotlin (Android) | GUI to deploy the binary, start/stop collection, set interface filters, toggle error capture. |
| **AIDL catalog builder** | Python | Builds a JSON catalog mapping `(interface, code)` → method name from a folder of AIDL (via generated stubs / `--dumpapi`). |
| **DLT Viewer plugin** | TBD | Decodes captured logs against the catalog inside DLT Viewer. |
| **VS Code plugin** | TypeScript | Same decoding, as a VS Code extension. |

### AIDL catalog (JSON)

A Python script takes a folder of AIDL (passed as a parameter) and emits a JSON
catalog. Rather than re-deriving code order by hand, it reads the **real transaction
constants** the AIDL compiler assigns (from generated stubs / `aidl --dumpapi`), so
explicit `= N` ids are honored. The catalog is the shared contract between the (dumb)
runtime logs and the (smart) viewer decoders. Shape is TBD, but conceptually:

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
- **Hybrid parcel handling.** The probe copies the raw descriptor bytes out of the
  (soon-freed) parcel and computes a bounded **hash** over them; **interpretation
  (UTF-16→UTF-8 decode, formatting) happens in Rust userspace**, where it is
  unit-testable with byte fixtures. The header offset is the only version-specific
  constant the probe needs.
- **In-kernel interface filter via descriptor hash.** The interface filter matches
  the in-kernel descriptor hash against a BPF map of wanted hashes, dropping
  unwanted events **before** the ring buffer — keeping the high-volume path cheap
  without full string parsing in the probe. (New in the rewrite; not in the PoC.)
- **AIDL catalog input: all `.aidl` files under a folder passed as a parameter.**
  The Python builder recurses a given directory. *Caveat:* to keep transaction codes
  aligned, that folder must be the AIDL that matches the device build.
- **Control channel: unix domain socket** between control app and consumer, with
  peer-credential (`SO_PEERCRED`) auth. *Caveat:* requires an SELinux policy rule for
  the app's domain to connect to the daemon socket.
- **Shared decoder core + thin plugins.** Catalog lookup + line parsing live in one
  plugin-agnostic core, exposed as a `bindfetto-decode` CLI; DLT Viewer and VS Code
  plugins are thin adapters over it. One viewer path shipped first.
- **Catalog built from aidl-generated stubs, not hand-rolled ordering.** The Python
  builder reads the real `TRANSACTION_* = FIRST_CALL_TRANSACTION + N` constants from
  generated stubs / `aidl --dumpapi`, so explicit `= N` ids are honored and AIDL
  numbering isn't re-implemented. Special transactions
  (INTERFACE/DUMP/PING/SHELL_COMMAND) handled explicitly; native (non-AIDL)
  interfaces are out of catalog coverage.
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

Still to decide:

- **Transaction-error attach point:** which binder tracepoint / return-path hook
  reliably surfaces `BR_FAILED_REPLY` / `BR_DEAD_REPLY` (the second attach point).
- **Control command protocol:** the command set carried over the unix socket
  (start/stop, filter, error toggle) and its encoding.
- **Binary deployment without signature permission:** the fallback path and where
  the privileged binary lives on device.
- **CO-RE portability** across kernels — deferred; the initial dev target is fixed
  (see below).

## Non-goals (initial)

- Not a passive analyzer only — but bindfetto does **not modify or block**
  transactions; it observes.
- Not a full profiler or tracing suite; scope is Binder IPC visibility.
- No cross-device aggregation/backend; capture and decode are local.
- Oneway (async) spam detection is deferred, not in the initial scope.
